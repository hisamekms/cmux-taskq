//! `stats` (ADR-0023 decision 5): the time each run spent in work,
//! validation, waiting to land and startup, how often it came back, and the
//! thresholds it crossed. Everything is derived from `run_events`; this
//! module is a pure function of the events, the task → goal map and a
//! snapshot of the supervisors' free slots.
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;
use serde_json::Value;

use super::{
    EventId, GoalId, RunEvent, RunId, RunStatus, TaskId,
    reason::{REPEATED_CODE_KINDS, event_code},
    stall::{BackgroundTask, StallConfig},
};

pub mod thresholds;

pub use thresholds::ThresholdStats;

/// Runs returned without `--full`.
pub const DEFAULT_RUNS: usize = 50;
/// A run waiting to land longer than this many seconds is an alert.
pub const AWAITING_INTEGRATION_SECS: i64 = 15 * 60;
/// The `needs_session` count of a run that is an alert.
pub const NEEDS_SESSION_TIMES: i64 = 3;
/// An ask open longer than this many seconds is an alert.
pub const ASK_UNANSWERED_SECS: i64 = 60 * 60;
/// The `failed` count across one task's runs that is an alert.
pub const TASK_FAILED_TIMES: i64 = 2;
/// A run whose work took more than this many times its goal's median is an alert.
pub const WORK_MEDIAN_FACTOR: i64 = 2;
/// This many `backend_call_failed` in one window is an alert.
pub const BACKEND_FAILURES: i64 = 2;

/// What `stats` looks at.
#[derive(Debug, Clone, Default)]
pub struct StatsQuery {
    /// Only runs that finished after this event id (`--since`).
    pub since: Option<EventId>,
    /// Only runs of tasks in this goal (`--goal`).
    pub goal_id: Option<GoalId>,
    /// Every finished run instead of [`DEFAULT_RUNS`] (`--full`).
    pub full: bool,
}

/// The supervisors as they are now: execution slots nobody uses, the
/// dependency-ready tasks and the ready tasks that are still blocked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotSnapshot {
    pub free_slots: i64,
    pub candidates: usize,
    pub ready: usize,
}

/// One run's times in seconds (null when an end point was never recorded)
/// and counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunStats {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub goal_id: Option<GoalId>,
    /// `integrated`, `failed` or `interrupted` for a finished run; the last
    /// status recorded for one still in flight.
    pub status: Option<String>,
    /// The event that finished the run; `--since` compares against it.
    pub finished_event_id: Option<EventId>,
    /// `run_claimed` → first `receipt_observed`.
    pub work: Option<i64>,
    /// First `receipt_observed` → first `validation_finished`.
    pub validate: Option<i64>,
    /// First `validation_finished` → `run_integrated`.
    pub wait_to_land: Option<i64>,
    /// `agent_started` → `first_commit_observed`.
    pub startup: Option<i64>,
    pub resumes: i64,
    /// The `verdict` of the last `review_finished`.
    pub review_verdict: Option<String>,
    pub needs_session: i64,
    pub failed: i64,
}

/// Count, sum and median of one interval over a set of runs; runs without
/// the interval are not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub count: usize,
    pub total: i64,
    pub median: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Intervals {
    pub runs: usize,
    pub work: Summary,
    pub validate: Summary,
    pub wait_to_land: Summary,
    pub startup: Summary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalStats {
    pub goal_id: Option<GoalId>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    pub kind: &'static str,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    pub value: i64,
    pub threshold: i64,
}

/// The `backend_call_failed` events in the window: how often cmux failed or
/// timed out, for which calls, and under what load. A window where nothing
/// failed has a zero count and null maxima.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct BackendFailures {
    pub count: i64,
    /// Failures per `op`.
    pub by_op: BTreeMap<String, i64>,
    /// The highest 1-minute load average recorded with a failure.
    pub max_load_avg: Option<f64>,
    /// The most slots held when one failed.
    pub max_slots: Option<i64>,
}

/// The events in the window that carry a reason code (ADR-0034): how often
/// each code was recorded, and in which kinds of event. An event whose code
/// repeats that of the `validation_finished` recorded with it
/// (`evidence_missing`, `scope_violation`) is not counted again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReasonCodes {
    pub count: i64,
    /// Events per code.
    pub by_code: BTreeMap<String, i64>,
    /// Events per kind, then per code.
    pub by_kind: BTreeMap<String, BTreeMap<String, i64>>,
}

/// The tasks canceled as duplicates in the window (ADR-0046 decision 5):
/// how many, and which task each duplicates, in event order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DuplicateCancels {
    pub count: i64,
    pub tasks: Vec<DuplicateCancel>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DuplicateCancel {
    pub task_id: TaskId,
    pub duplicate_of: TaskId,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stats {
    /// Finished runs, oldest finish first.
    pub runs: Vec<RunStats>,
    /// One entry per goal of those runs, ascending; runs of no goal last.
    pub goals: Vec<GoalStats>,
    pub overall: Intervals,
    pub alerts: Vec<Alert>,
    /// Failed backend calls after `--since` (up to `next_cursor`); without
    /// it, those since the earliest first event of the runs returned, or all of
    /// them with `--full` or when no run is returned. With `--goal`, only
    /// the failures of that goal's runs.
    pub backend_failures: BackendFailures,
    /// The reason codes recorded in the same window as `backend_failures`.
    pub reason_codes: ReasonCodes,
    /// The tasks canceled as duplicates in the same window as `backend_failures`.
    pub duplicate_cancels: DuplicateCancels,
    /// The runs not finished yet that look stalled now (ADR-0043 decision
    /// 5), whatever `--since` says; with `--goal`, only that goal's.
    pub running_alerts: Vec<RunningAlert>,
    /// Whether `workspace_mismatch` could be judged: cmux's workspaces
    /// were listed, or why not.
    pub workspace_check: WorkspaceCheck,
    /// The thresholds `running_alerts` were judged by, and where they came from.
    pub stall_config: StallConfigReport,
    /// Per `[stall]` setting (ADR-0043 decision 6): the detections made in
    /// the same window as `backend_failures`, how each ended, how long it
    /// took, the people who stepped in before any detection, and the
    /// running alerts it judges now.
    pub stall_thresholds: BTreeMap<&'static str, ThresholdStats>,
    /// Pass it to `--since` to read only runs that finish later.
    pub next_cursor: EventId,
}

/// An alert about a run still in flight (ADR-0043 decision 5): its
/// `value` and `threshold` are seconds, except for `workspace_mismatch`,
/// which has neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunningAlert {
    /// `idle_without_receipt`, `long_background`, `running_outlier` or
    /// `workspace_mismatch`.
    pub kind: &'static str,
    pub task_id: Option<TaskId>,
    pub run_id: Option<RunId>,
    /// The watched session (`session`, `resume` or `revise`) of an
    /// `idle_without_receipt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i64>,
    /// `run_without_workspace` or `workspace_without_run` for a
    /// `workspace_mismatch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// The worker workspace no unfinished run owns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// For `idle_without_receipt`: whether the supervisor nudged this
    /// session (`stall_nudged`) and has a `stalled` ask open for the run.
    /// Neither means the supervisor missed it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nudged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asked: Option<bool>,
    /// The background tasks the idle marker lists as running.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub background_tasks: Vec<BackgroundTask>,
}

impl RunningAlert {
    fn new(kind: &'static str, task_id: Option<TaskId>, run_id: Option<RunId>) -> Self {
        Self {
            kind,
            task_id,
            run_id,
            phase: None,
            value: None,
            threshold: None,
            reason: None,
            workspace_id: None,
            nudged: None,
            asked: None,
            background_tasks: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WorkspaceCheck {
    /// cmux listed this many workspaces.
    Checked { workspaces: usize },
    /// cmux could not be asked; no `workspace_mismatch` is judged.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StallConfigReport {
    #[serde(flatten)]
    pub config: StallConfig,
    /// `supervisor` (the latest `stall_config_loaded`), `file` (the
    /// `[stall]` of `dagq.toml`) or `default`.
    pub source: &'static str,
}

/// A workspace cmux lists: its stable ID and its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedWorkspace {
    pub id: String,
    pub description: Option<String>,
}

/// cmux's workspaces, or why they could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspaces {
    Listed(Vec<ListedWorkspace>),
    Unavailable(String),
}

/// What `stats` reads of a run not finished yet outside the queue: the
/// files in its run directory, as unix milliseconds of their last write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun {
    pub run_id: RunId,
    pub task_id: TaskId,
    pub status: RunStatus,
    /// The worker workspace the run was given.
    pub workspace_id: Option<String>,
    /// The idle marker, with the background tasks it lists as running.
    pub idle: Option<(i64, Vec<BackgroundTask>)>,
    pub receipt: Option<i64>,
    /// The last input the session took (the agent's prompt-submit
    /// marker), for an agent that writes one.
    pub input: Option<i64>,
    /// When the longest running of the idle marker's background tasks was
    /// first listed, when the hook's history shows it earlier than the
    /// marker; the marker's time otherwise.
    pub background_since: Option<i64>,
}

/// The state `running_alerts` are judged on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSnapshot {
    /// The queue's runs not finished yet.
    pub runs: Vec<LiveRun>,
    /// Every run of the queue, with its task: a workspace naming one of
    /// them belongs to this queue.
    pub known_runs: HashMap<RunId, TaskId>,
    /// The inbox's, planner's and supervisor's workspaces (`session_workspaces`).
    pub session_workspaces: Vec<String>,
    pub workspaces: Workspaces,
    /// The queue's hash, which its worker workspaces' descriptions carry.
    pub queue_hash: String,
    pub config: StallConfigReport,
}

impl Default for LiveSnapshot {
    /// No run in flight, no workspace listed, the default thresholds.
    fn default() -> Self {
        Self {
            runs: Vec::new(),
            known_runs: HashMap::new(),
            session_workspaces: Vec::new(),
            workspaces: Workspaces::Unavailable("not listed".to_owned()),
            queue_hash: String::new(),
            config: StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        }
    }
}

/// Aggregate `events` (every `run_events` row, ascending id). `goals` maps a
/// task to its goal, `now` is the unix second the open waits are measured
/// to, `slots` is the supervisors' snapshot for the idle alert and `live`
/// what the running alerts are judged on.
pub fn stats(
    events: &[RunEvent],
    goals: &HashMap<TaskId, Option<GoalId>>,
    now: i64,
    slots: SlotSnapshot,
    query: &StatsQuery,
    live: &LiveSnapshot,
) -> Stats {
    let latest = events.iter().map(|e| e.id).max().unwrap_or(EventId::new(0));
    let in_goal = |task_id: TaskId| {
        query
            .goal_id
            .is_none_or(|goal| goals.get(&task_id).copied().flatten() == Some(goal))
    };
    let tracks = runs(events, goals);
    let running_alerts: Vec<RunningAlert> = running_alerts(events, &tracks, now, live)
        .into_iter()
        .filter(|alert| alert.task_id.is_none_or(in_goal))
        .collect();
    let workspace_check = match &live.workspaces {
        Workspaces::Listed(workspaces) => WorkspaceCheck::Checked {
            workspaces: workspaces.len(),
        },
        Workspaces::Unavailable(reason) => WorkspaceCheck::Unavailable {
            reason: reason.clone(),
        },
    };
    let mut first_event: HashMap<&str, EventId> = HashMap::new();
    for event in events {
        if let Some(run_id) = &event.run_id {
            first_event.entry(run_id.as_str()).or_insert(event.id);
        }
    }
    // Per task, over every run and not only this page: the `failed` count
    // and the latest run that failed.
    let mut failures: BTreeMap<TaskId, (i64, RunId)> = BTreeMap::new();
    for track in tracks.iter().filter(|track| track.stats.failed > 0) {
        let entry = failures
            .entry(track.stats.task_id)
            .or_insert_with(|| (0, track.stats.run_id.clone()));
        entry.0 += track.stats.failed;
        entry.1.clone_from(&track.stats.run_id);
    }
    let (finished, open): (Vec<_>, Vec<_>) = tracks
        .into_iter()
        .filter(|track| in_goal(track.stats.task_id))
        .partition(|track| track.stats.finished_event_id.is_some());
    let mut finished = finished
        .into_iter()
        .filter(|track| {
            query
                .since
                .is_none_or(|since| track.stats.finished_event_id > Some(since))
        })
        .collect::<Vec<_>>();
    finished.sort_by_key(|track| track.stats.finished_event_id);
    let limit = if query.full {
        finished.len()
    } else {
        DEFAULT_RUNS
    };
    let mut next_cursor = latest;
    if finished.len() > limit {
        if query.since.is_some() {
            // Page forward: the oldest runs past the cursor, then the rest.
            finished.truncate(limit);
            next_cursor = finished
                .last()
                .and_then(|track| track.stats.finished_event_id)
                .unwrap_or(latest);
        } else {
            finished.drain(..finished.len() - limit);
        }
    }

    let mut by_goal: BTreeMap<(bool, Option<GoalId>), Vec<&RunStats>> = BTreeMap::new();
    for track in &finished {
        let goal = track.stats.goal_id;
        by_goal
            .entry((goal.is_none(), goal))
            .or_default()
            .push(&track.stats);
    }
    let goal_stats = by_goal
        .iter()
        .map(|(&(_, goal_id), runs)| GoalStats {
            goal_id,
            intervals: intervals(runs),
        })
        .collect::<Vec<_>>();
    let overall = intervals(&finished.iter().map(|t| &t.stats).collect::<Vec<_>>());

    let mut alerts = Vec::new();
    let considered = finished.iter().chain(open.iter()).collect::<Vec<_>>();
    for track in &considered {
        let run = &track.stats;
        let waited = match (run.wait_to_land, track.awaiting_since) {
            (Some(wait), _) => Some(wait),
            (None, Some(since)) if run.status.as_deref() == Some("awaiting_integration") => {
                Some((now * 1000 - since) / 1000)
            }
            _ => None,
        };
        if let Some(waited) = waited.filter(|&w| w > AWAITING_INTEGRATION_SECS) {
            alerts.push(alert(
                "awaiting_integration",
                run,
                waited,
                AWAITING_INTEGRATION_SECS,
            ));
        }
        if run.needs_session >= NEEDS_SESSION_TIMES {
            alerts.push(alert(
                "needs_session",
                run,
                run.needs_session,
                NEEDS_SESSION_TIMES,
            ));
        }
        let median = goal_stats
            .iter()
            .find(|goal| goal.goal_id == run.goal_id)
            .and_then(|goal| goal.intervals.work.median);
        if let (Some(work), Some(median)) = (run.work, median)
            && median > 0
            && work > median * WORK_MEDIAN_FACTOR
        {
            alerts.push(alert(
                "work_over_median",
                run,
                work,
                median * WORK_MEDIAN_FACTOR,
            ));
        }
    }
    // Once the latest failed run of a task is on this page (or in flight).
    for track in &considered {
        if let Some((count, run_id)) = failures.get(&track.stats.task_id)
            && *run_id == track.stats.run_id
            && *count >= TASK_FAILED_TIMES
        {
            alerts.push(alert(
                "task_failed",
                &track.stats,
                *count,
                TASK_FAILED_TIMES,
            ));
        }
    }
    for ask in open_asks(events) {
        let waited = (now * 1000 - ask.opened_ms) / 1000;
        if waited > ASK_UNANSWERED_SECS && ask.task_id.is_none_or(in_goal) {
            alerts.push(Alert {
                kind: "ask_unanswered",
                task_id: ask.task_id,
                run_id: ask.run_id,
                value: waited,
                threshold: ASK_UNANSWERED_SECS,
            });
        }
    }
    let window_start = match query.since {
        Some(since) => since,
        None if query.full => EventId::new(0),
        None => finished
            .iter()
            .filter_map(|track| first_event.get(track.stats.run_id.as_str()))
            .min()
            .map_or(EventId::new(0), |id| EventId::new(id.as_i64() - 1)),
    };
    let counts = |task_id: Option<TaskId>| query.goal_id.is_none() || task_id.is_some_and(in_goal);
    let backend_failures = backend_failures(events, window_start, next_cursor, counts);
    let reason_codes = reason_codes(events, window_start, next_cursor, counts);
    let duplicate_cancels = duplicate_cancels(events, window_start, next_cursor, counts);
    let stall_thresholds = thresholds::thresholds(
        &thresholds::detections(events, now * 1000),
        &thresholds::preemptions(events),
        |id, task_id| id > window_start && id <= next_cursor && counts(task_id),
        &running_alerts,
        &live.config.config,
    );
    if backend_failures.count >= BACKEND_FAILURES {
        alerts.push(Alert {
            kind: "backend_failures",
            task_id: None,
            run_id: None,
            value: backend_failures.count,
            threshold: BACKEND_FAILURES,
        });
    }
    if slots.free_slots > 0 && slots.candidates == 0 && slots.ready > 0 {
        alerts.push(Alert {
            kind: "idle_slots",
            task_id: None,
            run_id: None,
            value: slots.free_slots,
            threshold: 0,
        });
    }

    Stats {
        runs: finished.into_iter().map(|track| track.stats).collect(),
        goals: goal_stats,
        overall,
        alerts,
        backend_failures,
        reason_codes,
        duplicate_cancels,
        running_alerts,
        workspace_check,
        stall_config: live.config.clone(),
        stall_thresholds,
        next_cursor,
    }
}

/// The session the supervisor watches for a run in `status` now, by its
/// events: `session` (the worker's own, from its latest `agent_started`),
/// `resume` (a `resume_started` with no end yet) or `revise` (a
/// `revise_requested` the session has not answered), with the unix
/// millisecond it started. `None` when no session is watched.
fn watched_phase(status: RunStatus, events: &[&RunEvent]) -> Option<(&'static str, i64)> {
    let latest = |kinds: &[&str]| {
        events
            .iter()
            .rev()
            .find(|event| kinds.contains(&event.kind.as_str()))
            .copied()
    };
    let at = |event: &RunEvent| timestamp_millis(&event.created_at);
    match status {
        RunStatus::Running => {
            let start = latest(&["agent_started"]).or_else(|| latest(&["run_claimed"]))?;
            Some(("session", at(start)?))
        }
        RunStatus::NeedsSession => {
            let event = latest(&["resume_started", "resume_finished", "resume_skipped"])?;
            (event.kind == "resume_started")
                .then(|| at(event).map(|start| ("resume", start)))
                .flatten()
        }
        RunStatus::Validating | RunStatus::AwaitingIntegration => {
            let event = latest(&[
                "validation_finished",
                "review_started",
                "review_finished",
                "revise_requested",
                "revise_finished",
                "revise_receipt_rejected",
                "conflict_precheck",
                "conflict_resolved",
            ])?;
            (event.kind == "revise_requested")
                .then(|| at(event).map(|start| ("revise", start)))
                .flatten()
        }
        _ => None,
    }
}

/// The run a worker workspace's description names, with the queue hash it
/// names: `dagq role=worker queue=<hash> run=<id> task=<id>` for the
/// worker's own workspace, `run <id> resume` (no hash) for a resume's.
fn described_run(description: &str) -> Option<(Option<&str>, &str)> {
    let words: Vec<&str> = description.split_whitespace().collect();
    match words.as_slice() {
        ["run", run, "resume"] => Some((None, run)),
        ["dagq", fields @ ..] => {
            let field = |name: &str| {
                fields
                    .iter()
                    .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
            };
            (field("role")? == "worker").then_some((field("queue"), field("run")?))
        }
        _ => None,
    }
}

/// The alerts about runs still in flight (ADR-0043 decision 5), judged at
/// `now` (unix seconds) on `live`.
fn running_alerts(
    events: &[RunEvent],
    tracks: &[Track],
    now: i64,
    live: &LiveSnapshot,
) -> Vec<RunningAlert> {
    let now_ms = now * 1000;
    let config = &live.config.config;
    let asks = open_asks(events);
    // The work medians over every finished run, per goal.
    let mut works: HashMap<Option<GoalId>, Vec<i64>> = HashMap::new();
    for track in tracks
        .iter()
        .filter(|t| t.stats.finished_event_id.is_some())
    {
        if let Some(work) = track.stats.work {
            works.entry(track.stats.goal_id).or_default().push(work);
        }
    }
    let medians: HashMap<Option<GoalId>, i64> = works
        .into_iter()
        .filter_map(|(goal, mut works)| Some((goal, median(&mut works)?)))
        .collect();
    let listed: Option<Vec<(&ListedWorkspace, Option<&str>)>> = match &live.workspaces {
        Workspaces::Listed(workspaces) => Some(
            workspaces
                .iter()
                .map(|workspace| {
                    let run = workspace
                        .description
                        .as_deref()
                        .and_then(described_run)
                        .filter(|(queue, run)| match queue {
                            Some(queue) => *queue == live.queue_hash,
                            None => live.known_runs.keys().any(|known| known.as_str() == *run),
                        })
                        .map(|(_, run)| run);
                    (workspace, run)
                })
                .collect(),
        ),
        Workspaces::Unavailable(_) => None,
    };
    let mut alerts = Vec::new();
    for run in &live.runs {
        let run_events: Vec<&RunEvent> = events
            .iter()
            .filter(|event| event.run_id.as_ref() == Some(&run.run_id))
            .collect();
        let since = |start: i64, kind: &str| {
            run_events.iter().any(|event| {
                event.kind == kind
                    && timestamp_millis(&event.created_at).is_some_and(|at| at >= start)
            })
        };
        let open_ask = |kinds: &[&str]| {
            asks.iter().any(|ask| {
                ask.run_id.as_ref() == Some(&run.run_id)
                    && ask
                        .kind
                        .as_deref()
                        .is_some_and(|kind| kinds.contains(&kind))
            })
        };
        let phase = watched_phase(run.status, &run_events);
        if let (Some((phase, start)), Some((idle, background))) = (phase, &run.idle) {
            let dialog = run_events
                .iter()
                .rev()
                .find(|event| matches!(event.kind.as_str(), "prompt_waiting" | "prompt_cleared"))
                .is_some_and(|event| {
                    event.kind == "prompt_waiting"
                        && timestamp_millis(&event.created_at).is_some_and(|at| at >= start)
                });
            let idle_secs = (now_ms - idle) / 1000;
            if *idle > start
                && run.receipt.is_none_or(|receipt| receipt <= start)
                && run.input.is_none_or(|input| input <= *idle)
                && !dialog
                && !open_ask(&["worker_question", "answer_prompt"])
                && idle_secs > config.idle_without_receipt_secs
            {
                let mut alert = RunningAlert::new(
                    "idle_without_receipt",
                    Some(run.task_id),
                    Some(run.run_id.clone()),
                );
                alert.phase = Some(phase);
                alert.value = Some(idle_secs);
                alert.threshold = Some(config.idle_without_receipt_secs);
                alert.nudged = Some(since(start, "stall_nudged"));
                alert.asked = Some(open_ask(&["stalled"]));
                alert.background_tasks.clone_from(background);
                alerts.push(alert);
            }
        }
        // Background work of a session that has not ended since the marker.
        // A session handed to validation alive (`session_live`, or a resume
        // finished into `validating`) has not ended (ADR-0027).
        let ended = |idle: i64| {
            run_events.iter().any(|event| {
                timestamp_millis(&event.created_at).is_some_and(|at| at >= idle)
                    && match event.kind.as_str() {
                        "workspace_closed" => true,
                        "supervision_finished" => event.payload["session_live"] != true,
                        "resume_finished" => {
                            event.payload["status"] != RunStatus::Validating.as_str()
                        }
                        _ => false,
                    }
            })
        };
        if let Some((idle, background)) = &run.idle
            && !background.is_empty()
            && !ended(*idle)
        {
            let since = run.background_since.map_or(*idle, |since| since.min(*idle));
            let running = (now_ms - since) / 1000;
            if running > config.background_alert_secs {
                let mut alert = RunningAlert::new(
                    "long_background",
                    Some(run.task_id),
                    Some(run.run_id.clone()),
                );
                alert.value = Some(running);
                alert.threshold = Some(config.background_alert_secs);
                alert.background_tasks.clone_from(background);
                alerts.push(alert);
            }
        }
        if run.status == RunStatus::Running
            && let Some(track) = tracks.iter().find(|t| t.stats.run_id == run.run_id)
            && let Some(claimed) = track.claimed
            && let Some(&median) = medians.get(&track.stats.goal_id)
            && median > 0
        {
            let running = (now_ms - claimed) / 1000;
            if running > median * WORK_MEDIAN_FACTOR {
                let mut alert = RunningAlert::new(
                    "running_outlier",
                    Some(run.task_id),
                    Some(run.run_id.clone()),
                );
                alert.value = Some(running);
                alert.threshold = Some(median * WORK_MEDIAN_FACTOR);
                alerts.push(alert);
            }
        }
        if let (Some(listed), Some(_)) = (&listed, phase) {
            let known: HashSet<&str> = run
                .workspace_id
                .iter()
                .map(String::as_str)
                .chain(
                    run_events
                        .iter()
                        .filter(|event| event.kind == "resume_finished")
                        .filter_map(|event| event.payload["workspace_id"].as_str()),
                )
                .collect();
            let open = listed.iter().any(|(workspace, owner)| {
                *owner == Some(run.run_id.as_str())
                    || known
                        .iter()
                        .any(|id| id.eq_ignore_ascii_case(&workspace.id))
            });
            if !open {
                let mut alert = RunningAlert::new(
                    "workspace_mismatch",
                    Some(run.task_id),
                    Some(run.run_id.clone()),
                );
                alert.reason = Some("run_without_workspace");
                alert.workspace_id.clone_from(&run.workspace_id);
                alerts.push(alert);
            }
        }
    }
    for (workspace, owner) in listed.iter().flatten() {
        let Some(owner) = owner else { continue };
        if live
            .session_workspaces
            .iter()
            .any(|id| id.eq_ignore_ascii_case(&workspace.id))
            || live.runs.iter().any(|run| run.run_id.as_str() == *owner)
        {
            continue;
        }
        let run_id = RunId::new(*owner).ok();
        let task_id = run_id
            .as_ref()
            .and_then(|run_id| live.known_runs.get(run_id).copied());
        let mut alert = RunningAlert::new("workspace_mismatch", task_id, run_id);
        alert.reason = Some("workspace_without_run");
        alert.workspace_id = Some(workspace.id.clone());
        alerts.push(alert);
    }
    alerts
}

/// Count the reason codes of the events with `after < id <= upto` whose
/// task `counts` accepts.
fn reason_codes(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ReasonCodes {
    let mut codes = ReasonCodes::default();
    for event in events.iter().filter(|event| {
        event.id > after
            && event.id <= upto
            && !REPEATED_CODE_KINDS.contains(&event.kind.as_str())
            && counts(event.task_id)
    }) {
        let Some(code) = event_code(event) else {
            continue;
        };
        codes.count += 1;
        *codes.by_code.entry(code.as_str().to_owned()).or_default() += 1;
        *codes
            .by_kind
            .entry(event.kind.clone())
            .or_default()
            .entry(code.as_str().to_owned())
            .or_default() += 1;
    }
    codes
}

/// The `task_status_changed` events with `after < id <= upto` that
/// canceled a task as a duplicate, whose task `counts` accepts.
fn duplicate_cancels(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> DuplicateCancels {
    let mut cancels = DuplicateCancels::default();
    for event in events.iter().filter(|event| {
        event.kind == "task_status_changed"
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
    }) {
        if let (Some(task_id), Some(duplicate_of)) = (
            event.task_id,
            event.payload.get("duplicate_of").and_then(Value::as_i64),
        ) {
            cancels.count += 1;
            cancels.tasks.push(DuplicateCancel {
                task_id,
                duplicate_of: TaskId::new(duplicate_of),
            });
        }
    }
    cancels
}

/// Aggregate the `backend_call_failed` events with `after < id <= upto`
/// whose task `counts` accepts.
fn backend_failures(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> BackendFailures {
    let mut failures = BackendFailures::default();
    for event in events.iter().filter(|event| {
        event.kind == "backend_call_failed"
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
    }) {
        failures.count += 1;
        let op = event.payload.get("op").and_then(Value::as_str);
        *failures
            .by_op
            .entry(op.unwrap_or("unknown").to_owned())
            .or_default() += 1;
        if let Some(load) = event.payload.get("load_avg").and_then(Value::as_f64) {
            failures.max_load_avg = Some(failures.max_load_avg.map_or(load, |max| max.max(load)));
        }
        if let Some(slots) = event.payload.get("slots").and_then(Value::as_i64) {
            failures.max_slots = Some(failures.max_slots.map_or(slots, |max| max.max(slots)));
        }
    }
    failures
}

fn alert(kind: &'static str, run: &RunStats, value: i64, threshold: i64) -> Alert {
    Alert {
        kind,
        task_id: Some(run.task_id),
        run_id: Some(run.run_id.clone()),
        value,
        threshold,
    }
}

/// The median of `values` (sorted in place); the lower-rounded mean of the
/// two middle values for an even count.
pub fn median(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    let n = values.len();
    match n {
        0 => None,
        _ if n % 2 == 1 => Some(values[n / 2]),
        _ => Some((values[n / 2 - 1] + values[n / 2]).div_euclid(2)),
    }
}

fn summary(values: impl Iterator<Item = Option<i64>>) -> Summary {
    let mut values = values.flatten().collect::<Vec<_>>();
    Summary {
        count: values.len(),
        total: values.iter().sum(),
        median: median(&mut values),
    }
}

fn intervals(runs: &[&RunStats]) -> Intervals {
    Intervals {
        runs: runs.len(),
        work: summary(runs.iter().map(|r| r.work)),
        validate: summary(runs.iter().map(|r| r.validate)),
        wait_to_land: summary(runs.iter().map(|r| r.wait_to_land)),
        startup: summary(runs.iter().map(|r| r.startup)),
    }
}

/// A run as its events describe it, with the instants (unix milliseconds)
/// the intervals are measured between.
struct Track {
    stats: RunStats,
    claimed: Option<i64>,
    receipt: Option<i64>,
    validated: Option<i64>,
    agent_started: Option<i64>,
    awaiting_since: Option<i64>,
}

fn payload_status(payload: &Value) -> Option<&str> {
    payload.get("status").and_then(Value::as_str)
}

fn seconds_between(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    Some((to? - from?) / 1000)
}

/// Group the run events by run, in order of each run's first event.
fn runs(events: &[RunEvent], goals: &HashMap<TaskId, Option<GoalId>>) -> Vec<Track> {
    let mut order: Vec<RunId> = Vec::new();
    let mut tracks: HashMap<RunId, Track> = HashMap::new();
    for event in events {
        let (Some(run_id), Some(task_id)) = (&event.run_id, event.task_id) else {
            continue;
        };
        let at = timestamp_millis(&event.created_at);
        let track = tracks.entry(run_id.clone()).or_insert_with(|| {
            order.push(run_id.clone());
            Track {
                stats: RunStats {
                    run_id: run_id.clone(),
                    task_id,
                    goal_id: goals.get(&task_id).copied().flatten(),
                    status: None,
                    finished_event_id: None,
                    work: None,
                    validate: None,
                    wait_to_land: None,
                    startup: None,
                    resumes: 0,
                    review_verdict: None,
                    needs_session: 0,
                    failed: 0,
                },
                claimed: None,
                receipt: None,
                validated: None,
                agent_started: None,
                awaiting_since: None,
            }
        });
        let run = &mut track.stats;
        match event.kind.as_str() {
            "run_claimed" => track.claimed = track.claimed.or(at),
            "agent_started" => track.agent_started = track.agent_started.or(at),
            "first_commit_observed" if run.startup.is_none() => {
                run.startup = seconds_between(track.agent_started, at);
            }
            "receipt_observed" if track.receipt.is_none() => {
                track.receipt = at;
                run.work = seconds_between(track.claimed, at);
            }
            "validation_finished" if track.validated.is_none() => {
                track.validated = at;
                run.validate = seconds_between(track.receipt, at);
            }
            "integration_started" => run.status = Some("integrating".to_owned()),
            "resume_started" => run.resumes += 1,
            "review_finished" => {
                run.review_verdict = event
                    .payload
                    .get("verdict")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            "run_integrated" => {
                run.wait_to_land = seconds_between(track.validated, at);
                run.status = Some("integrated".to_owned());
                run.finished_event_id.get_or_insert(event.id);
            }
            _ => {}
        }
        if let Some(status) = payload_status(&event.payload) {
            run.status = Some(status.to_owned());
            match status {
                // `integration_error` only puts back the status the run had
                // before the attempt, and `resume_finished` reports the run
                // still parked (ADR-0019); neither parks anything new.
                "needs_session"
                    if !matches!(event.kind.as_str(), "integration_error" | "resume_finished") =>
                {
                    run.needs_session += 1
                }
                "failed" => run.failed += 1,
                _ => {}
            }
            // The wait starts at the first `awaiting_integration`, like
            // `wait_to_land`; going back there after an error keeps it.
            if status == "awaiting_integration" && track.awaiting_since.is_none() {
                track.awaiting_since = at;
            }
            if matches!(status, "failed" | "interrupted") {
                run.finished_event_id.get_or_insert(event.id);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| tracks.remove(&id))
        .collect()
}

struct OpenAsk {
    task_id: Option<TaskId>,
    run_id: Option<RunId>,
    /// The ask's kind, as `ask_opened` recorded it.
    kind: Option<String>,
    opened_ms: i64,
}

/// Asks (ADR-0022) whose `ask_opened` has no `ask_answered` yet. The two
/// events are paired by the payload's `ask_id` (or `id`), and by run and
/// task when neither is recorded.
fn open_asks(events: &[RunEvent]) -> Vec<OpenAsk> {
    let key = |event: &RunEvent| {
        let id = event
            .payload
            .get("ask_id")
            .or_else(|| event.payload.get("id"))
            .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned));
        (id, event.task_id, event.run_id.clone())
    };
    let mut open: Vec<(_, OpenAsk)> = Vec::new();
    for event in events {
        match event.kind.as_str() {
            "ask_opened" => {
                if let Some(opened_ms) = timestamp_millis(&event.created_at) {
                    open.push((
                        key(event),
                        OpenAsk {
                            task_id: event.task_id,
                            run_id: event.run_id.clone(),
                            kind: event
                                .payload
                                .get("kind")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            opened_ms,
                        },
                    ));
                }
            }
            "ask_answered" => {
                let answered = key(event);
                open.retain(|(opened, _)| *opened != answered);
            }
            _ => {}
        }
    }
    open.into_iter().map(|(_, ask)| ask).collect()
}

/// Unix milliseconds of a queue timestamp `YYYY-MM-DDTHH:MM:SS[.fff]Z`
/// (SQLite's `strftime('%Y-%m-%dT%H:%M:%fZ')`); `None` when it does not parse.
pub fn timestamp_millis(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z').unwrap_or(text);
    let (date, time) = text.split_once(['T', ' '])?;
    let mut date = date.splitn(3, '-').map(|part| part.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let mut time = time.splitn(3, ':');
    let hour = time.next()?.parse::<i64>().ok()?;
    let minute = time.next()?.parse::<i64>().ok()?;
    let seconds = time.next()?;
    let (second, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    let second = second.parse::<i64>().ok()?;
    let millis = format!("{fraction:0<3}").get(..3)?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, task_id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task_id)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    const T: i64 = 1_800_000_000;
    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";
    const R3: &str = "33333333-3333-4333-8333-333333333333";

    fn at(secs: i64) -> String {
        crate::application::timestamp(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
        )
    }

    fn run_event(id: i64, run: &str, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            run_id: Some(RunId::new(run).unwrap()),
            created_at: at(secs),
            ..event(id, 1, kind, payload)
        }
    }

    fn live_run(run: &str, status: RunStatus) -> LiveRun {
        LiveRun {
            run_id: RunId::new(run).unwrap(),
            task_id: TaskId::new(1),
            status,
            workspace_id: Some(format!("ws-{run}")),
            idle: None,
            receipt: None,
            input: None,
            background_since: None,
        }
    }

    fn snapshot(runs: Vec<LiveRun>, workspaces: Workspaces) -> LiveSnapshot {
        LiveSnapshot {
            known_runs: [R1, R2, R3]
                .iter()
                .map(|run| (RunId::new(*run).unwrap(), TaskId::new(1)))
                .collect(),
            runs,
            session_workspaces: vec!["WS-INBOX".to_owned()],
            workspaces,
            queue_hash: "hash".to_owned(),
            config: StallConfigReport {
                config: StallConfig::default(),
                source: "default",
            },
        }
    }

    fn running(events: &[RunEvent], live: &LiveSnapshot, now: i64) -> Vec<RunningAlert> {
        stats(
            events,
            &HashMap::new(),
            now,
            SlotSnapshot::default(),
            &StatsQuery::default(),
            live,
        )
        .running_alerts
    }

    fn cargo_test() -> Vec<BackgroundTask> {
        vec![BackgroundTask {
            id: "b1".into(),
            description: "cargo test".into(),
            command: "cargo test --locked".into(),
        }]
    }

    /// Task 182: the worker stopped waiting for a background `cargo test`
    /// that never came back, with no receipt.
    #[test]
    fn an_idle_session_without_a_receipt_is_an_alert_with_its_background_work() {
        let events = [
            run_event(1, R1, "run_claimed", json!({}), T),
            run_event(2, R1, "agent_started", json!({}), T + 10),
        ];
        let mut run = live_run(R1, RunStatus::Running);
        run.idle = Some(((T + 60) * 1000, cargo_test()));
        let live = snapshot(vec![run.clone()], Workspaces::Unavailable("none".into()));
        // Under the threshold: nothing yet.
        assert!(running(&events, &live, T + 60 + 1200).is_empty());
        let alerts = running(&events, &live, T + 60 + 1300);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        let alert = &alerts[0];
        assert_eq!(alert.kind, "idle_without_receipt");
        assert_eq!(alert.phase, Some("session"));
        assert_eq!((alert.value, alert.threshold), (Some(1300), Some(1200)));
        assert_eq!((alert.nudged, alert.asked), (Some(false), Some(false)));
        assert_eq!(alert.background_tasks, cargo_test());
        // 10.5 hours later the background work is an alert of its own.
        let kinds: Vec<_> = running(&events, &live, T + 60 + 37_800)
            .iter()
            .map(|alert| alert.kind)
            .collect();
        assert_eq!(kinds, ["idle_without_receipt", "long_background"]);

        // A nudge and an open `stalled` ask show the supervisor saw it.
        let mut seen = events.to_vec();
        seen.push(run_event(3, R1, "stall_nudged", json!({}), T + 1300));
        seen.push(run_event(
            4,
            R1,
            "ask_opened",
            json!({"ask_id": 7, "kind": "stalled"}),
            T + 2600,
        ));
        let alert = &running(&seen, &live, T + 2700)[0];
        assert_eq!((alert.nudged, alert.asked), (Some(true), Some(true)));
    }

    #[test]
    fn a_receipt_an_input_a_question_or_a_dialog_is_not_a_stall() {
        let now = T + 5000;
        let events = [run_event(1, R1, "agent_started", json!({}), T)];
        let idle = |run: &mut LiveRun| run.idle = Some(((T + 60) * 1000, Vec::new()));
        let alerts = |events: &[RunEvent], run: LiveRun| {
            running(
                events,
                &snapshot(vec![run], Workspaces::Unavailable("none".into())),
                now,
            )
        };
        let mut run = live_run(R1, RunStatus::Running);
        idle(&mut run);
        assert_eq!(alerts(&events, run.clone()).len(), 1);
        // No marker, no idle.
        assert!(alerts(&events, live_run(R1, RunStatus::Running)).is_empty());
        // A receipt of this session.
        let mut with_receipt = run.clone();
        with_receipt.receipt = Some((T + 50) * 1000);
        assert!(alerts(&events, with_receipt).is_empty());
        // An input taken after the marker: it works again.
        let mut with_input = run.clone();
        with_input.input = Some((T + 70) * 1000);
        assert!(alerts(&events, with_input).is_empty());
        // A marker of an earlier session.
        let mut earlier = run.clone();
        earlier.idle = Some(((T - 60) * 1000, Vec::new()));
        assert!(alerts(&events, earlier).is_empty());
        // A question to the inbox, or a dialog, waits for a person.
        for (kind, payload) in [
            (
                "ask_opened",
                json!({"ask_id": 1, "kind": "worker_question"}),
            ),
            ("ask_opened", json!({"ask_id": 1, "kind": "answer_prompt"})),
            ("prompt_waiting", json!({"prompt": "choice"})),
        ] {
            let mut held = events.to_vec();
            held.push(run_event(2, R1, kind, payload, T + 30));
            assert!(alerts(&held, run.clone()).is_empty(), "{kind}");
        }
        // Once the dialog is cleared or the question answered, it is one again.
        let mut cleared = events.to_vec();
        cleared.push(run_event(2, R1, "prompt_waiting", json!({}), T + 30));
        cleared.push(run_event(3, R1, "prompt_cleared", json!({}), T + 40));
        cleared.push(run_event(
            4,
            R1,
            "ask_opened",
            json!({"ask_id": 2, "kind": "worker_question"}),
            T + 41,
        ));
        cleared.push(run_event(
            5,
            R1,
            "ask_answered",
            json!({"ask_id": 2}),
            T + 42,
        ));
        assert_eq!(alerts(&cleared, run).len(), 1);
    }

    #[test]
    fn resumed_and_revised_sessions_are_watched_from_their_request() {
        let now = T + 5000;
        let resume = [
            run_event(1, R1, "agent_started", json!({}), T - 9000),
            run_event(2, R1, "resume_started", json!({"attempt": 1}), T),
        ];
        let mut run = live_run(R1, RunStatus::NeedsSession);
        run.idle = Some(((T + 60) * 1000, Vec::new()));
        // An older receipt does not answer the resume.
        run.receipt = Some((T - 100) * 1000);
        let live = snapshot(vec![run.clone()], Workspaces::Unavailable("none".into()));
        let alerts = running(&resume, &live, now);
        assert_eq!(alerts[0].phase, Some("resume"), "{alerts:?}");
        let mut finished = resume.to_vec();
        finished.push(run_event(
            3,
            R1,
            "resume_finished",
            json!({"status": "needs_session"}),
            T + 30,
        ));
        assert!(running(&finished, &live, now).is_empty());

        let revise = [
            run_event(1, R1, "validation_finished", json!({}), T - 100),
            run_event(2, R1, "revise_requested", json!({"attempt": 1}), T),
        ];
        run.status = RunStatus::AwaitingIntegration;
        let live = snapshot(vec![run], Workspaces::Unavailable("none".into()));
        assert_eq!(running(&revise, &live, now)[0].phase, Some("revise"));
        let mut answered = revise.to_vec();
        answered.push(run_event(3, R1, "revise_finished", json!({}), T + 30));
        assert!(running(&answered, &live, now).is_empty());
    }

    #[test]
    fn background_work_is_timed_from_when_it_was_first_listed() {
        let mut run = live_run(R1, RunStatus::Running);
        run.idle = Some((T * 1000, cargo_test()));
        run.background_since = Some((T - 1000) * 1000);
        let live = snapshot(vec![run.clone()], Workspaces::Unavailable("none".into()));
        let alerts = running(&[], &live, T + 1000);
        let background: Vec<_> = alerts
            .iter()
            .filter(|alert| alert.kind == "long_background")
            .collect();
        assert_eq!(background.len(), 1, "{alerts:?}");
        assert_eq!(background[0].value, Some(2000));
        // Timed from the marker alone, it is under the threshold.
        run.background_since = None;
        let live = snapshot(vec![run], Workspaces::Unavailable("none".into()));
        assert!(
            !running(&[], &live, T + 1000)
                .iter()
                .any(|alert| alert.kind == "long_background")
        );
    }

    #[test]
    fn background_work_of_an_ended_session_is_no_alert() {
        let mut run = live_run(R1, RunStatus::AwaitingIntegration);
        run.idle = Some((T * 1000, cargo_test()));
        run.receipt = Some((T - 10) * 1000);
        let live = snapshot(vec![run], Workspaces::Unavailable("none".into()));
        let events = [run_event(1, R1, "validation_finished", json!({}), T - 5)];
        let alerts = running(&events, &live, T + 1801);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "long_background");
        assert_eq!(alerts[0].phase, None);
        // Handed to validation with the session alive (the B type of task 242):
        // still running.
        let mut live_session = events.to_vec();
        live_session.push(run_event(
            2,
            R1,
            "supervision_finished",
            json!({"session_live": true}),
            T + 5,
        ));
        live_session.push(run_event(
            3,
            R1,
            "resume_finished",
            json!({"status": "validating"}),
            T + 6,
        ));
        assert_eq!(running(&live_session, &live, T + 1801).len(), 1);
        for (kind, payload) in [
            ("supervision_finished", json!({"exit_code": 0})),
            ("resume_finished", json!({"status": "needs_session"})),
            ("workspace_closed", json!({})),
        ] {
            let mut ended = events.to_vec();
            ended.push(run_event(2, R1, kind, payload, T + 5));
            assert!(running(&ended, &live, T + 1801).is_empty(), "{kind}");
        }
    }

    #[test]
    fn a_running_run_past_twice_its_goals_median_is_an_outlier() {
        let mut goals = HashMap::new();
        goals.insert(TaskId::new(1), Some(GoalId::new(9)));
        let mut events = Vec::new();
        for (index, run) in [R2, R3].iter().enumerate() {
            let base = index as i64 * 10;
            events.push(run_event(base + 1, run, "run_claimed", json!({}), T));
            events.push(run_event(
                base + 2,
                run,
                "receipt_observed",
                json!({}),
                T + 100,
            ));
            events.push(run_event(
                base + 3,
                run,
                "run_integrated",
                json!({}),
                T + 200,
            ));
        }
        events.push(run_event(30, R1, "run_claimed", json!({}), T + 1000));
        let live = snapshot(
            vec![live_run(R1, RunStatus::Running)],
            Workspaces::Unavailable("none".into()),
        );
        let alerts = |now| {
            stats(
                &events,
                &goals,
                now,
                SlotSnapshot::default(),
                &StatsQuery::default(),
                &live,
            )
            .running_alerts
        };
        assert!(alerts(T + 1200).is_empty());
        let outlier = alerts(T + 1300);
        assert_eq!(outlier.len(), 1);
        assert_eq!(outlier[0].kind, "running_outlier");
        assert_eq!(
            (outlier[0].value, outlier[0].threshold),
            (Some(300), Some(200))
        );
        // `--goal` of another goal leaves it out.
        let other = StatsQuery {
            goal_id: Some(GoalId::new(1)),
            ..StatsQuery::default()
        };
        assert!(
            stats(
                &events,
                &goals,
                T + 1300,
                SlotSnapshot::default(),
                &other,
                &live
            )
            .running_alerts
            .is_empty()
        );
    }

    #[test]
    fn workspaces_and_unfinished_runs_that_do_not_match_are_alerts() {
        let workspace = |id: &str, description: Option<String>| ListedWorkspace {
            id: id.to_owned(),
            description,
        };
        let listed = vec![
            // R1 runs in its own workspace (listed in another case).
            workspace(&format!("WS-{R1}"), None),
            // R2 is resumed: its workspace is known by its description.
            workspace("WS-RESUME", Some(format!("run {R2} resume"))),
            // R3 finished, and its worker workspace is still open.
            workspace(
                "WS-LEFT",
                Some(format!("dagq role=worker queue=hash run={R3} task=1")),
            ),
            // Another queue's worker, the inbox, a person's own workspace.
            workspace(
                "WS-OTHER",
                Some(format!("dagq role=worker queue=other run={R3} task=1")),
            ),
            workspace("ws-inbox", Some("dagq role=inbox queue=hash".into())),
            workspace("WS-MINE", None),
        ];
        let events = [
            run_event(1, R1, "agent_started", json!({}), T),
            run_event(2, R2, "resume_started", json!({}), T),
        ];
        let runs = vec![
            live_run(R1, RunStatus::Running),
            live_run(R2, RunStatus::NeedsSession),
        ];
        let result = stats(
            &events,
            &HashMap::new(),
            T + 10,
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &snapshot(runs.clone(), Workspaces::Listed(listed.clone())),
        );
        assert_eq!(
            result.workspace_check,
            WorkspaceCheck::Checked { workspaces: 6 }
        );
        let alerts = result.running_alerts;
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0].kind, "workspace_mismatch");
        assert_eq!(alerts[0].reason, Some("workspace_without_run"));
        assert_eq!(alerts[0].workspace_id.as_deref(), Some("WS-LEFT"));
        assert_eq!(alerts[0].run_id.as_ref().map(RunId::as_str), Some(R3));
        assert_eq!(alerts[0].task_id, Some(TaskId::new(1)));

        // Neither workspace of R1 and R2 is open any more.
        let alerts = running(
            &events,
            &snapshot(runs.clone(), Workspaces::Listed(listed[2..].to_vec())),
            T + 10,
        );
        let missing: Vec<_> = alerts
            .iter()
            .filter(|alert| alert.reason == Some("run_without_workspace"))
            .map(|alert| alert.run_id.as_ref().unwrap().as_str())
            .collect();
        assert_eq!(missing, [R1, R2]);

        // Without cmux, nothing is judged about workspaces.
        let result = stats(
            &events,
            &HashMap::new(),
            T + 10,
            SlotSnapshot::default(),
            &StatsQuery::default(),
            &snapshot(runs, Workspaces::Unavailable("cmux is gone".into())),
        );
        assert!(result.running_alerts.is_empty());
        assert_eq!(
            serde_json::to_value(&result.workspace_check).unwrap(),
            json!({"status": "unavailable", "reason": "cmux is gone"})
        );
        assert_eq!(
            serde_json::to_value(&result.stall_config).unwrap(),
            json!({"idle_without_receipt_secs": 1200, "send_confirm_secs": 60, "background_alert_secs": 1800, "source": "default"})
        );
        assert_eq!(described_run("dagq role=worker queue=hash"), None);
        assert_eq!(described_run("run x"), None);
    }

    #[test]
    fn reason_codes_are_counted_once_per_park_within_the_window() {
        let events = [
            event(
                1,
                1,
                "supervision_finished",
                json!({"code": "session_killed"}),
            ),
            event(
                2,
                1,
                "validation_finished",
                json!({"code": "evidence_missing"}),
            ),
            event(
                3,
                1,
                "evidence_missing",
                json!({"code": "evidence_missing"}),
            ),
            event(
                4,
                2,
                "integration_deferred",
                json!({"code": "rebase_conflict"}),
            ),
            event(
                5,
                1,
                "runtime_error",
                json!({"message": "before the codes"}),
            ),
            event(
                6,
                1,
                "supervision_finished",
                json!({"code": "session_killed"}),
            ),
            event(
                7,
                1,
                "backend_call_failed",
                json!({"code": "backend_failed"}),
            ),
        ];
        let codes = reason_codes(&events, EventId::new(0), EventId::new(5), |_| true);
        assert_eq!(codes.count, 3);
        assert_eq!(
            codes.by_code,
            BTreeMap::from([
                ("evidence_missing".to_owned(), 1),
                ("rebase_conflict".to_owned(), 1),
                ("session_killed".to_owned(), 1),
            ])
        );
        assert_eq!(
            codes.by_kind["integration_deferred"],
            BTreeMap::from([("rebase_conflict".to_owned(), 1)])
        );
        assert_eq!(
            reason_codes(&events, EventId::new(5), EventId::new(7), |_| true).count,
            1
        );
        let task_two = reason_codes(&events, EventId::new(0), EventId::new(6), |task| {
            task == Some(TaskId::new(2))
        });
        assert_eq!(task_two.count, 1);
    }

    /// `stall_thresholds` counts the detections made after `--since`, of
    /// the goal's tasks only, and the running alerts judged now.
    #[test]
    fn stall_thresholds_follow_the_window_and_the_goal() {
        let nudged = json!({"phase": "session", "idle_secs": 1250, "threshold_secs": 1200});
        let resolved = json!({
            "phase": "session", "detection": "nudge", "threshold": "idle_without_receipt_secs",
            "threshold_secs": 1200, "detected_after_secs": 1250,
            "outcome": "resolved_by_nudge", "resolved_after_secs": 60,
        });
        let events = vec![
            run_event(1, R1, "agent_started", json!({}), T),
            run_event(2, R1, "stall_nudged", nudged.clone(), T + 1250),
            run_event(3, R1, "stall_resolved", resolved, T + 1310),
            run_event(4, R2, "agent_started", json!({}), T),
            RunEvent {
                task_id: Some(TaskId::new(2)),
                ..run_event(5, R2, "stall_nudged", nudged, T + 1250)
            },
        ];
        let mut run = live_run(R1, RunStatus::Running);
        run.idle = Some(((T + 1320) * 1000, Vec::new()));
        let live = snapshot(vec![run], Workspaces::Unavailable("none".into()));
        let at = |query: &StatsQuery, goals: &HashMap<TaskId, Option<GoalId>>| {
            stats(
                &events,
                goals,
                T + 3000,
                SlotSnapshot::default(),
                query,
                &live,
            )
        };
        let all = at(&StatsQuery::default(), &HashMap::new());
        let idle = &all.stall_thresholds["idle_without_receipt_secs"];
        assert_eq!(idle.detections, 2);
        assert_eq!(idle.outcomes["resolved_by_nudge"], 1);
        assert_eq!(idle.outcomes["pending"], 1);
        assert_eq!(idle.running_alerts, 1);
        let json = serde_json::to_value(&all).unwrap();
        assert_eq!(
            json["stall_thresholds"]["idle_without_receipt_secs"]["by_detection"]["nudge"]["count"],
            2
        );
        assert_eq!(
            json["stall_thresholds"]["send_confirm_secs"]["threshold_secs"],
            60
        );
        // After the first nudge only the second counts.
        let since = at(
            &StatsQuery {
                since: Some(EventId::new(3)),
                ..StatsQuery::default()
            },
            &HashMap::new(),
        );
        assert_eq!(
            since.stall_thresholds["idle_without_receipt_secs"].outcomes,
            BTreeMap::from([("pending".to_owned(), 1)])
        );
        // Goal 1 has task 1 only.
        let goals = HashMap::from([
            (TaskId::new(1), Some(GoalId::new(1))),
            (TaskId::new(2), Some(GoalId::new(2))),
        ]);
        let goal = at(
            &StatsQuery {
                goal_id: Some(GoalId::new(1)),
                full: true,
                ..StatsQuery::default()
            },
            &goals,
        );
        assert_eq!(
            goal.stall_thresholds["idle_without_receipt_secs"].outcomes,
            BTreeMap::from([("resolved_by_nudge".to_owned(), 1)])
        );
    }
}
