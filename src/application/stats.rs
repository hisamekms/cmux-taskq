//! `stats` (ADR-0023 decision 5): the queue's reads that
//! [`crate::domain::stats::stats`] derives the per-run and per-goal times
//! and the thresholds crossed from, and what the running alerts (ADR-0043
//! decision 5) read outside the queue: the run directories' markers and
//! cmux's workspaces; and main's Git history for `conflict_hotspots`.

use std::{path::Path, time::SystemTime};

use anyhow::Result;

use super::{AgentSignals, ProcessControl, Queue, RunFiles, StatusFilter, TaskQuery};
use crate::domain::{
    GoalStatus, RunStatus, SessionRole, SupervisorPulse, TaskRun, TaskStatus,
    stall::{BackgroundTask, IDLE_LOG, StallConfig, background_first_seen},
    stats::{
        ConflictConfig, ConflictConfigReport, History, ListedWorkspace, LiveRun, LiveSnapshot,
        SlotSnapshot, StallConfigReport, Stats, StatsQuery, Workspaces,
        conflicts::{MainHistory, earliest_conflict},
        stats as aggregate, timestamp_millis, with_kinds,
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
    /// The `[conflicts]` of `dagq.toml`; `None` when there is no file.
    pub conflicts_file: &'a dyn Fn() -> Result<Option<ConflictConfig>>,
    /// Main's history since a unix second (see
    /// [`crate::application::Repository::main_history`]).
    pub history: &'a dyn Fn(i64) -> Result<MainHistory>,
}

/// Main's history since the earliest event of `events` (a second before
/// it), so that the landings of any window before its first conflict are
/// counted too, or why it could not be read; none is read without a
/// conflict.
pub fn conflict_history(
    events: &[crate::domain::RunEvent],
    read: &dyn Fn(i64) -> Result<MainHistory>,
) -> History {
    let earliest = events
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min();
    match earliest_conflict(events).and(earliest) {
        None => History::Read(MainHistory::default()),
        Some(millis) => match read(millis.div_euclid(1000) - 1) {
            Ok(history) => History::Read(history),
            Err(error) => History::Unavailable(format!("{error:#}")),
        },
    }
}

/// The `[conflicts]` thresholds and where they came from.
pub fn conflict_config(file: Option<ConflictConfig>) -> ConflictConfigReport {
    match file {
        Some(config) => ConflictConfigReport {
            config,
            source: "file",
        },
        None => ConflictConfigReport::default(),
    }
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
        history: conflict_history(&events, sources.history),
        conflicts: conflict_config((sources.conflicts_file)()?),
    };
    let mut stats = aggregate(&events, &goals, now, snapshot, query, &live);
    let titles = queue.task_titles()?;
    for run in &mut stats.runs {
        run.title = titles.get(&run.task_id).cloned();
    }
    with_kinds(&mut stats, &queue.task_kinds()?);
    Ok(stats)
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
    let (idle, background_since, input) = match run.run_dir() {
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
            let since = match &idle {
                Some((at, tasks)) if !tasks.is_empty() => {
                    background_since(sources, &Path::new(dir).join(IDLE_LOG), *at, tasks)?
                }
                _ => None,
            };
            (
                idle,
                since,
                modified(&Path::new(dir).join(PROMPT_SUBMIT_MARKER)),
            )
        }
        None => (None, None, None),
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
        background_since,
    })
}

/// When the longest running of the marker's background `tasks` was first
/// listed: the earliest of their first appearances in the unbroken streak
/// of markers in the hook's `log` that ends with the marker written `at`.
/// A task the log does not show (no log, as from a session started before
/// the hook wrote one) is timed from the marker. `None` when the log shows
/// none earlier than the marker.
fn background_since(
    sources: &StatsSources<'_>,
    log: &Path,
    at: i64,
    tasks: &[BackgroundTask],
) -> Result<Option<i64>> {
    let Some((_, bytes)) = sources.files.read_stamped(log)? else {
        return Ok(None);
    };
    let history = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (secs, marker) = line.split_once('\t')?;
            let secs: i64 = secs.trim().parse().ok()?;
            Some((
                secs.saturating_mul(1000),
                sources
                    .signals
                    .idle_hook(marker.as_bytes())
                    .background_tasks,
            ))
        })
        // The marker itself ends the streak, whether or not its line made it.
        .chain([(at, tasks.to_vec())])
        .collect::<Vec<_>>();
    let seen = background_first_seen(history);
    Ok(tasks
        .iter()
        .filter_map(|task| seen.get(&task.id).copied())
        .min()
        .filter(|&since| since < at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, RunEvent, TaskId};
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: serde_json::Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: at.to_owned(),
        }
    }

    /// The history is read from the earliest event, not the earliest
    /// conflict, and only when there is a conflict; a failure to read it
    /// is reported, not raised.
    #[test]
    fn conflict_history_reads_from_the_earliest_event() {
        let quiet = [event(
            1,
            "run_claimed",
            json!({}),
            "1970-01-01T00:01:40.000Z",
        )];
        let never = |_: i64| -> Result<MainHistory> { unreachable!() };
        assert_eq!(
            conflict_history(&quiet, &never),
            History::Read(MainHistory::default())
        );
        let events = [
            quiet[0].clone(),
            event(
                2,
                "conflict_precheck",
                json!({"main": "m", "conflicts": ["a"]}),
                "1970-01-01T00:03:20.000Z",
            ),
        ];
        let since = std::cell::Cell::new(0);
        let read = |at: i64| {
            since.set(at);
            Ok(MainHistory::default())
        };
        conflict_history(&events, &read);
        assert_eq!(since.get(), 99);
        let failing = |_: i64| -> Result<MainHistory> { anyhow::bail!("no git") };
        assert_eq!(
            conflict_history(&events, &failing),
            History::Unavailable("no git".into())
        );
        assert_eq!(conflict_config(None).source, "default");
        assert_eq!(
            conflict_config(Some(ConflictConfig::default())).source,
            "file"
        );
    }
}
