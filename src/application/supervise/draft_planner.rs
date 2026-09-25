//! Planners the runtime opens for drafts (ADR-0041 decision 16, which
//! replaced the follow-up triage job of ADR-0037): every draft the runtime
//! or a job registered (a follow_up of a landed receipt, a goal's gap) gets
//! a planner of the runtime's, within [`LoopSettings::runtime_planners`]
//! (shared with the planners opened for revises), oldest draft first. The
//! planner adopts the draft (completes and submits it, so plan review
//! checks it), drops it (cancels it with a note) or asks the inbox a
//! `planner_question`, whose answer the supervisor types into its
//! workspace (or hands to a new planner when that one is gone). A planner
//! that ends with the draft undecided is followed by another, at most
//! [`crate::domain::MAX_DRAFT_PLANNERS`] per draft.

use super::*;
use crate::{
    application::{
        DraftPlannerStart, PlannerAnswerRoute,
        planner::{PlannerView, open_draft_planner},
        prompt::{DraftPlannerMaterial, draft_planner_prompt},
    },
    domain::{Ask, DraftOrigin, DraftTarget, PlannerState},
};

impl Supervisor<'_> {
    /// Deliver the answered `planner_question` asks: typed into the live
    /// planner that works on the ask's task once it stopped after asking,
    /// handed to a new planner when the draft's one is gone (within the
    /// limit, counted in `runtime_open`), or closed when the draft moved on.
    /// A typing that fails records `ask_delivery_failed` and leaves the
    /// answer to the inbox.
    pub(super) fn deliver_planner_answers(
        &mut self,
        options: &LoopSettings,
        views: &[PlannerView],
        runtime_open: &mut usize,
    ) -> Result<()> {
        for ask in self.queue.planner_answers()? {
            let Some(task) = ask.task_id else {
                continue;
            };
            match self.queue.planner_answer_route(&ask)? {
                PlannerAnswerRoute::Planner(planner) => {
                    let Some(view) = views.iter().find(|view| view.planner.id == planner.id) else {
                        continue;
                    };
                    // The planner that asked is typed to once it stopped
                    // after asking; a question someone else opened about
                    // its draft waits only for it to be idle.
                    let asked_by_planner = ask.asked_by == SessionRole::Planner.as_str();
                    if view.state != PlannerState::Idle
                        || (asked_by_planner
                            && view.idle_since.is_none_or(|since| since < ask.created_at))
                    {
                        continue;
                    }
                    let Some(workspace) = view.planner.workspace_id.clone() else {
                        continue;
                    };
                    if self.delivery_failed(task, ask.id)? {
                        continue;
                    }
                    let text = format!(
                        "answer to ask {}: {}",
                        ask.id,
                        ask.answer.as_deref().unwrap_or_default()
                    );
                    match submit_input(self.cmux, self.signals, &workspace, Input::Text(&text)) {
                        Ok(_) => {
                            self.queue.ask_delivered(ask.id, &workspace)?;
                            info!(task_id = %task, ask_id = %ask.id, "answer of ask {} sent to planner {} in workspace {workspace}", ask.id, planner.id);
                        }
                        Err(error) => {
                            warn!(task_id = %task, ask_id = %ask.id, error = %format_args!("{error:#}"), "answer of ask {} could not be sent to planner {} in workspace {workspace}: {error:#}; it is left to the inbox", ask.id, planner.id);
                            self.queue.record_task_event(
                                task,
                                "ask_delivery_failed",
                                json!({
                                    "ask_id": ask.id,
                                    "workspace_id": workspace,
                                    "planner_id": planner.id,
                                    "error": format!("{error:#}"),
                                }),
                            )?;
                        }
                    }
                }
                PlannerAnswerRoute::NewPlanner if *runtime_open < options.runtime_planners => {
                    if let Some(workspace) = self.start_draft_planner(task, Some(&ask))? {
                        *runtime_open += 1;
                        self.queue.ask_delivered(ask.id, &workspace)?;
                    }
                }
                // At the limit: the answer waits for a planner to end.
                PlannerAnswerRoute::NewPlanner => {}
                PlannerAnswerRoute::Close => {
                    self.queue.close_planner_answer(
                        ask.id,
                        "its draft moved on and no planner of the runtime's works on it",
                    )?;
                    info!(task_id = %task, ask_id = %ask.id, "ask {} closed: draft task {task} moved on", ask.id);
                }
                PlannerAnswerRoute::Person => {}
            }
        }
        Ok(())
    }

    fn delivery_failed(&mut self, task: TaskId, ask: AskId) -> Result<bool> {
        Ok(self.queue.show(task)?.events.iter().any(|event| {
            event.kind == "ask_delivery_failed"
                && event.payload.get("ask_id").and_then(Value::as_i64) == Some(ask.as_i64())
        }))
    }

    /// Open a planner for each draft waiting for one, oldest first, while
    /// the runtime's planners are below the limit.
    pub(super) fn open_draft_planners(
        &mut self,
        options: &LoopSettings,
        runtime_open: &mut usize,
    ) -> Result<()> {
        for target in self.queue.planner_drafts()? {
            if *runtime_open >= options.runtime_planners {
                break;
            }
            if self.start_draft_planner(target.task.id(), None)?.is_some() {
                *runtime_open += 1;
            }
        }
        Ok(())
    }

    /// Record a planner for `draft` (carrying `answer`), write its prompt
    /// and open its workspace; the workspace's UUID, or `None` when the
    /// draft was not taken (another supervisor took it, it moved on, or its
    /// planners are used up).
    fn start_draft_planner(
        &mut self,
        draft: TaskId,
        answer: Option<&Ask>,
    ) -> Result<Option<String>> {
        let (planner, target, attempt) = match self
            .queue
            .open_draft_planner(draft, answer.map(|ask| ask.id))?
        {
            DraftPlannerStart::Skipped => return Ok(None),
            DraftPlannerStart::Exhausted { attempts } => {
                warn!(task_id = %draft, "draft task {draft}: {attempts} planners of the runtime's ended without deciding it; the inbox is told");
                return Ok(None);
            }
            DraftPlannerStart::Opened {
                planner,
                target,
                attempt,
            } => (planner, target, attempt),
        };
        let prompt = match self.draft_planner_material(&target, attempt, answer) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.queue
                    .close_planner(planner.id, Some(&format!("{error:#}")))?;
                return Err(error);
            }
        };
        let id = planner.id;
        let opened = open_draft_planner(&self.planner_launch(), planner, &prompt)?;
        let workspace = opened.planner.workspace_id.clone().unwrap_or_default();
        info!(task_id = %draft, "draft task {draft} ({}): opened planner {id} {attempt} in workspace {workspace}", target.origin.as_str());
        Ok(Some(workspace))
    }

    /// The initial prompt of the planner for `target`, from the queue as it
    /// is now.
    fn draft_planner_material(
        &mut self,
        target: &DraftTarget,
        attempt: usize,
        answer: Option<&Ask>,
    ) -> Result<String> {
        let source = match target
            .material
            .get("source_task_id")
            .and_then(Value::as_i64)
        {
            Some(id) => Some(self.queue.show(TaskId::new(id))?.task),
            None => None,
        };
        let receipt = match (
            target.origin,
            target.material.get("source_run_id").and_then(Value::as_str),
        ) {
            (DraftOrigin::FollowUp, Some(run)) => self
                .queue
                .run_events(&RunId::new(run)?)?
                .into_iter()
                .rev()
                .find(|event| event.kind == "integration_receipt")
                .map(|event| event.payload["receipt"].clone()),
            _ => None,
        };
        let goal = match target.task.goal_id() {
            Some(id) => Some(self.queue.show_goal(id)?),
            None => None,
        };
        let siblings: Vec<_> = goal
            .iter()
            .flat_map(|detail| detail.tasks.iter())
            .filter(|task| task.id != target.task.id())
            .cloned()
            .collect();
        draft_planner_prompt(&DraftPlannerMaterial {
            db: &self.layout.db,
            target,
            attempt,
            source: source.as_ref(),
            receipt: receipt.as_ref(),
            goal: goal.as_ref().map(|detail| &detail.goal),
            goal_closed: goal.as_ref().is_some_and(|detail| detail.closed),
            siblings: &siblings,
            answer,
        })
    }
}
