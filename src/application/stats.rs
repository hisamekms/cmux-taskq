//! `stats` (ADR-0023 decision 5): the queue's reads that
//! [`crate::domain::stats::stats`] derives the per-run and per-goal times
//! and the thresholds crossed from, and what the running alerts (ADR-0043
//! decision 5) read outside the queue: the run directories' markers and
//! cmux's workspaces.

use std::{path::Path, time::SystemTime};

use anyhow::Result;

use super::{AgentSignals, ProcessControl, Queue, RunFiles, StatusFilter, TaskQuery};
use crate::domain::{
    GoalStatus, RunStatus, SessionRole, SupervisorPulse, TaskRun, TaskStatus,
    stall::StallConfig,
    stats::{
        ListedWorkspace, LiveRun, LiveSnapshot, SlotSnapshot, StallConfigReport, Stats, StatsQuery,
        Workspaces, stats as aggregate,
    },
};

/// The marker the agent's `UserPromptSubmit` hook writes when the session
/// takes an input (ADR-0043 decision 2), in the run directory.
pub const PROMPT_SUBMIT_MARKER: &str = "prompt-submit.json";

/// The workspaces cmux has open, for `workspace_mismatch`.
pub trait WorkspaceListing {
    fn list_workspaces(&self) -> Result<Vec<ListedWorkspace>>;
}

/// What `stats` reads outside the queue.
pub struct StatsSources<'a> {
    pub files: &'a dyn RunFiles,
    pub signals: &'a dyn AgentSignals,
    /// `None` when there is no cmux to ask.
    pub workspaces: Option<&'a dyn WorkspaceListing>,
    /// The queue's hash, which its worker workspaces carry.
    pub queue_hash: &'a str,
    /// The `[stall]` of `dagq.toml`; `None` when there is no file.
    pub config_file: &'a dyn Fn() -> Result<Option<StallConfig>>,
}

/// Per-run and per-goal times and the thresholds crossed, derived from
/// `run_events`. The idle alert looks at the live supervisors' slots at
/// `now`, `processes` telling which are alive. The running alerts read the
/// unfinished runs' directories and cmux's workspaces through `sources`;
/// a cmux that cannot be asked leaves only `workspace_mismatch` unjudged.
/// Reads only.
pub fn stats(
    queue: &dyn Queue,
    processes: &dyn ProcessControl,
    now: i64,
    query: &StatsQuery,
    sources: &StatsSources<'_>,
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
    let config = match StallConfig::loaded(&events) {
        Some(config) => StallConfigReport {
            config,
            source: "supervisor",
        },
        None => match (sources.config_file)()? {
            Some(config) => StallConfigReport {
                config,
                source: "file",
            },
            None => StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        },
    };
    let all_runs = queue.all_runs()?;
    let mut runs = Vec::new();
    for run in all_runs.iter().filter(|run| !finished(run.status())) {
        runs.push(live_run(run, sources)?);
    }
    let mut session_workspaces = Vec::new();
    for role in [
        SessionRole::Supervisor,
        SessionRole::Inbox,
        SessionRole::Planner,
    ] {
        session_workspaces.extend(queue.session_workspace(role)?);
    }
    let workspaces = match sources.workspaces {
        None => Workspaces::Unavailable("no cmux to list the workspaces".to_owned()),
        Some(listing) => match listing.list_workspaces() {
            Ok(workspaces) => Workspaces::Listed(workspaces),
            Err(error) => Workspaces::Unavailable(format!("{error:#}")),
        },
    };
    let live = LiveSnapshot {
        runs,
        known_runs: all_runs
            .iter()
            .map(|run| (run.id().clone(), run.task_id()))
            .collect(),
        session_workspaces,
        workspaces,
        queue_hash: sources.queue_hash.to_owned(),
        config,
    };
    Ok(aggregate(&events, &goals, now, snapshot, query, &live))
}

fn finished(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Integrated | RunStatus::Succeeded | RunStatus::Failed | RunStatus::Interrupted
    )
}

fn millis(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The unfinished `run` with the write times of its idle marker, receipt
/// and prompt-submit marker. A file that is not there, or a run without a
/// directory, has none.
fn live_run(run: &TaskRun, sources: &StatsSources<'_>) -> Result<LiveRun> {
    let modified = |path: &Path| {
        sources
            .files
            .is_file(path)
            .then(|| sources.files.modified(path).ok().map(millis))
            .flatten()
    };
    let (idle, input) = match run.run_dir() {
        Some(dir) => {
            let idle = sources
                .files
                .read_stamped(&Path::new(dir).join("idle.json"))?
                .map(|(modified, bytes)| {
                    (
                        millis(modified),
                        sources.signals.idle_hook(&bytes).background_tasks,
                    )
                });
            (idle, modified(&Path::new(dir).join(PROMPT_SUBMIT_MARKER)))
        }
        None => (None, None),
    };
    Ok(LiveRun {
        run_id: run.id().clone(),
        task_id: run.task_id(),
        status: run.status(),
        workspace_id: run.workspace_id().map(str::to_owned),
        idle,
        receipt: run
            .receipt_path()
            .and_then(|path| modified(Path::new(path))),
        input,
    })
}
