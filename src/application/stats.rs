//! `stats` (ADR-0023 decision 5): the queue's reads that
//! [`crate::domain::stats::stats`] derives the per-run and per-goal times
//! and the thresholds crossed from.

use anyhow::Result;

use super::{ProcessControl, Queue, StatusFilter, TaskQuery};
use crate::domain::{
    GoalStatus, RunStatus, SupervisorPulse, TaskStatus,
    stats::{SlotSnapshot, Stats, StatsQuery, stats as aggregate},
};

/// Per-run and per-goal times and the thresholds crossed, derived from
/// `run_events`. The idle alert looks at the live supervisors' slots at
/// `now`, `processes` telling which are alive. Reads only.
pub fn stats(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    now: i64,
    query: &StatsQuery,
) -> Result<Stats> {
    let events = queue.all_events()?;
    let goals = queue.task_goals()?;
    let registrations = queue.supervisors()?;
    let slots: i64 = registrations
        .iter()
        .filter(|registration| {
            !SupervisorPulse::judge(registration, processes.alive(registration.pid), now).stale
        })
        .map(|registration| i64::from(registration.parallel))
        .sum();
    let executing = queue
        .active_runs()?
        .iter()
        .filter(|run| run.status() != RunStatus::Integrating)
        .count();
    let ready = queue
        .list(&TaskQuery {
            status: StatusFilter::Only(vec![TaskStatus::Ready]),
            limit: 1,
            ..Default::default()
        })?
        .total;
    // A draft goal's ready tasks wait for `goal ready`, not for a
    // predecessor, so they do not make free slots an alert.
    let ready_in_draft_goals: usize = queue
        .list_goals()?
        .iter()
        .filter(|goal| goal.status == GoalStatus::Draft)
        .map(|goal| goal.tasks.ready)
        .sum();
    let ready = ready.saturating_sub(ready_in_draft_goals);
    let snapshot = SlotSnapshot {
        free_slots: slots - i64::try_from(executing)?,
        candidates: queue.candidates()?.len(),
        ready,
    };
    Ok(aggregate(&events, &goals, now, snapshot, query))
}
