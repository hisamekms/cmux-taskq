//! Deferring the claim of a task whose files meet a run in flight on a
//! conflict hotspot (ADR-0069); the judgement is
//! [`crate::domain::claim_defer`]. What the judgement reads is cached: the
//! hotspots for [`HOT_REFRESH_SECS`], with the files each task is expected
//! to touch, and the files of the runs in flight for
//! [`IN_FLIGHT_REFRESH_SECS`] or until the next claim.
use std::collections::{HashMap, HashSet};

use anyhow::Result;
use tracing::{info, warn};

use super::Supervisor;
use crate::application::DependencyGraph;
use crate::domain::{
    Priority, TaskId,
    claim_defer::{
        self, DEFERRAL_KINDS, Decision, Deferral, InFlight, RELATED_TASKS, deferrals_in_place,
        expected_files,
    },
};

/// How long the hotspots (and the expected files of the tasks) are reused.
const HOT_REFRESH_SECS: i64 = 600;
/// How long the files of the runs in flight are reused, when no claim
/// comes first.
const IN_FLIGHT_REFRESH_SECS: i64 = 60;

/// What the supervisor keeps between passes.
#[derive(Default)]
pub(super) struct DeferWatch {
    /// The alerted hotspots, and when they were read.
    hot: Option<(i64, Vec<String>)>,
    /// The expected files of each task read since the hotspots were.
    expected: HashMap<TaskId, Vec<String>>,
    /// The runs in flight and their files, and when they were read.
    in_flight: Option<(i64, Vec<InFlight>)>,
    /// The deferrals in place; `None` until read from the queue.
    deferrals: Option<HashMap<TaskId, Deferral>>,
}

impl DeferWatch {
    /// A claim just made: the next judgement reads the runs in flight
    /// again, with the claimed one.
    pub(super) fn claimed(&mut self) {
        self.in_flight = None;
    }
}

impl Supervisor<'_> {
    /// The candidates of `graph` this pass may claim, in its order: those
    /// whose files meet a run in flight on a hotspot are passed over
    /// (ADR-0069), and the start and end of each deferral are recorded on
    /// its task.
    pub(super) fn claimable(&mut self, graph: &DependencyGraph) -> Result<Vec<TaskId>> {
        let now = self.generators.clock.now();
        let max_secs = self.conflicts.config.defer_max_secs;
        let (hot, in_flight) = self.hot_in_flight(now)?;
        let mut deferrals = match self.defer.deferrals.take() {
            Some(deferrals) => deferrals,
            None => deferrals_in_place(&self.queue.latest_task_events(&DEFERRAL_KINDS)?),
        };
        let mut order = Vec::new();
        let mut events = Vec::new();
        for &id in &graph.candidates {
            let interrupt = graph
                .tasks
                .iter()
                .find(|node| node.id == id)
                .is_some_and(|node| node.effective_priority == Priority::Interrupt);
            let overlap = if hot.is_empty() || interrupt {
                None
            } else {
                let expected = self.expected(id)?;
                claim_defer::overlap(&hot, &expected, &in_flight)
            };
            let mut deferral = deferrals.get(&id).copied();
            let decision = claim_defer::decide(
                interrupt,
                overlap,
                &mut deferral,
                now,
                max_secs,
                &self.token,
            );
            match deferral {
                Some(deferral) => deferrals.insert(id, deferral),
                None => deferrals.remove(&id),
            };
            match decision {
                Decision::Claim { event } => {
                    events.extend(event.map(|event| (id, event)));
                    order.push(id);
                }
                Decision::Defer { event } => events.extend(event.map(|event| (id, event))),
            }
        }
        let candidates: HashSet<TaskId> = graph.candidates.iter().copied().collect();
        deferrals.retain(|id, deferral| {
            if candidates.contains(id) {
                return true;
            }
            events.extend(claim_defer::left(*deferral, now, &self.token).map(|event| (*id, event)));
            false
        });
        self.defer.deferrals = Some(deferrals);
        for (id, (kind, payload)) in events {
            match kind {
                claim_defer::CLAIM_DEFERRED => warn!(
                    task_id = %id,
                    "claim of task {id} deferred: {}",
                    payload["message"].as_str().unwrap_or_default()
                ),
                _ => info!(
                    task_id = %id,
                    "claim of task {id} no longer deferred ({})",
                    payload["why"].as_str().unwrap_or_default()
                ),
            }
            self.queue.record_task_event(id, kind, payload)?;
        }
        Ok(order)
    }

    /// The hotspots some run in flight touches, and the runs in flight;
    /// both empty when no hotspot is alerted.
    fn hot_in_flight(&mut self, now: i64) -> Result<(Vec<String>, Vec<InFlight>)> {
        if self
            .defer
            .hot
            .as_ref()
            .is_none_or(|(at, _)| now - at >= HOT_REFRESH_SECS)
        {
            let hot = match self.conflict_hotspot_files() {
                Ok(files) => files
                    .into_iter()
                    .filter(|file| file.alert)
                    .map(|file| file.renamed_to.unwrap_or(file.path))
                    .collect(),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "the conflict hotspots could not be read: {error:#}; no claim is deferred on them");
                    Vec::new()
                }
            };
            self.defer.hot = Some((now, hot));
            self.defer.expected.clear();
            self.defer.in_flight = None;
        }
        let hot = self.defer.hot.as_ref().map(|(_, hot)| hot.clone());
        let hot = hot.unwrap_or_default();
        if hot.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        if self
            .defer
            .in_flight
            .as_ref()
            .is_none_or(|(at, _)| now - at >= IN_FLIGHT_REFRESH_SECS)
        {
            let in_flight = self.runs_in_flight()?;
            self.defer.in_flight = Some((now, in_flight));
        }
        let in_flight = self
            .defer
            .in_flight
            .as_ref()
            .map(|(_, runs)| runs.clone())
            .unwrap_or_default();
        let touched = hot
            .into_iter()
            .filter(|path| {
                in_flight
                    .iter()
                    .any(|run| claim_defer::touches(&run.files, path))
            })
            .collect();
        Ok((touched, in_flight))
    }

    /// The runs in flight (the latest run of each in-progress task) and
    /// the files each is expected to touch: its diff from its base to its
    /// head and the expected files of its task (ADR-0069 decision 2).
    pub(super) fn runs_in_flight(&mut self) -> Result<Vec<InFlight>> {
        let mut in_flight = Vec::new();
        for run in self.queue.latest_runs_in_progress()? {
            let mut files = self.expected(run.task_id())?;
            let head = run
                .result_commit()
                .map(|commit| commit.as_str().to_owned())
                .or_else(|| run.branch().map(str::to_owned));
            if let Some(head) = head {
                match self
                    .repository
                    .changed_paths(run.base_commit().as_str(), &head)
                {
                    Ok(changed) => files.extend(changed),
                    Err(error) => {
                        warn!(run_id = %run.id(), error = %format_args!("{error:#}"), "the files run {} changed could not be read: {error:#}", run.id())
                    }
                }
            }
            in_flight.push(InFlight {
                run_id: run.id().as_str().to_owned(),
                task_id: run.task_id(),
                files,
            });
        }
        Ok(in_flight)
    }

    /// The files `task` is expected to touch as it is now, not as cached:
    /// for a task whose paths may have changed since (one under plan
    /// review).
    pub(super) fn expected_now(&mut self, task: TaskId) -> Result<Vec<String>> {
        self.defer.expected.remove(&task);
        self.expected(task)
    }

    /// The files `task` is expected to touch: its declared paths, or the
    /// files its most related landed tasks changed.
    pub(super) fn expected(&mut self, task: TaskId) -> Result<Vec<String>> {
        if let Some(files) = self.defer.expected.get(&task) {
            return Ok(files.clone());
        }
        let declared = self.queue.show(task)?.task.paths().to_vec();
        let mut changed = Vec::new();
        if declared.is_empty() {
            match self.queue.related_landed_commits(task, RELATED_TASKS) {
                Ok(commits) => {
                    for commit in commits {
                        match self
                            .repository
                            .changed_paths(&format!("{commit}^"), &commit)
                        {
                            Ok(paths) => changed.extend(paths),
                            Err(error) => {
                                warn!(error = %format_args!("{error:#}"), "the files commit {commit} changed could not be read: {error:#}")
                            }
                        }
                    }
                }
                Err(error) => {
                    warn!(task_id = %task, error = %format_args!("{error:#}"), "the tasks related to task {task} could not be read: {error:#}")
                }
            }
        }
        let files = expected_files(&declared, &changed);
        self.defer.expected.insert(task, files.clone());
        Ok(files)
    }
}
