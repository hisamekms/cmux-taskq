//! Use cases and the ports they reach the outside through (ADR-0013):
//! `ports` holds the traits the infrastructure implements, `integrate` the
//! landing of a validated run, `prompt` the worker's prompt and the
//! initial prompts of the inbox and the planner, `review` the review
//! material of a run, `rebind` the binding to a moved repository and
//! `stats` the reads behind the run and goal times. The query types and
//! the dependency view of `list` and `graph` stay here.

pub mod ask;
pub mod health;
pub mod integrate;
pub mod naming;
mod ports;
pub mod prompt;
pub mod rebind;
pub mod recording;
pub mod review;
pub mod session;
pub mod stats;
pub mod supervise;

pub use ports::*;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::domain::{
    EvidenceCheck, GoalId, GoalStatus, RunId, RunStatus, Task, TaskId, TaskStatus,
};

/// Which task statuses `list` returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StatusFilter {
    /// Every status except the terminal ones (completed, canceled).
    #[default]
    Open,
    /// Every status, terminal ones included.
    Any,
    /// Only these statuses (any of them).
    Only(Vec<TaskStatus>),
}

/// Filter and page of a task listing. Filters combine with AND; tasks come
/// newest first (ID descending).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskQuery {
    pub status: StatusFilter,
    pub goal_id: Option<GoalId>,
    /// Page size; at least one.
    pub limit: usize,
    /// Start the page at this task ID: only tasks whose ID is at most this.
    /// The previous page's `next` is the first task of the following page.
    pub before: Option<TaskId>,
    /// Include description, acceptance, context, verification commands and timestamps.
    pub full: bool,
}

impl TaskQuery {
    pub const DEFAULT_LIMIT: usize = 20;
}

impl Default for TaskQuery {
    fn default() -> Self {
        Self {
            status: StatusFilter::Open,
            goal_id: None,
            limit: Self::DEFAULT_LIMIT,
            before: None,
            full: false,
        }
    }
}

/// One page of tasks. `next` is the ID of the first task past this page,
/// to pass as `before` for the following page, and null on the last one;
/// `total` counts every task the filter matches, regardless of the page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskPage {
    pub tasks: Vec<TaskListItem>,
    pub next: Option<TaskId>,
    pub total: usize,
}

/// A task as `list` shows it: what the planner decides on, plus the
/// long fields only with `full`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListItem {
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// IDs of the direct predecessors, ascending.
    pub dependencies: Vec<TaskId>,
    /// The most recently created run, if any.
    pub latest_run: Option<LatestRun>,
    #[serde(flatten)]
    pub details: Option<TaskListDetails>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LatestRun {
    pub id: RunId,
    pub status: RunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskListDetails {
    pub description: String,
    pub acceptance: String,
    pub verification_commands: Vec<String>,
    pub required_evidence: Vec<EvidenceCheck>,
    pub paths: Vec<String>,
    pub context: String,
    pub created_at: String,
    pub updated_at: String,
}

impl TaskListItem {
    pub fn new(
        task: Task,
        dependencies: Vec<TaskId>,
        latest_run: Option<LatestRun>,
        full: bool,
    ) -> Self {
        let details = full.then_some(TaskListDetails {
            description: task.description().to_owned(),
            acceptance: task.acceptance().to_owned(),
            verification_commands: task.verification_commands().to_vec(),
            required_evidence: task.required_evidence().to_vec(),
            paths: task.paths().to_vec(),
            context: task.context().to_owned(),
            created_at: task.created_at().to_owned(),
            updated_at: task.updated_at().to_owned(),
        });
        Self {
            id: task.id(),
            status: task.status(),
            title: task.title().to_owned(),
            goal_id: task.goal_id(),
            dependencies,
            latest_run,
            details,
        }
    }
}

/// An unfinished task (draft, ready or in progress) with every direct
/// predecessor, finished or not: the input of [`dependency_graph`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphTask {
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// Status of the task's goal; a draft goal's tasks are not candidates.
    pub goal_status: Option<GoalStatus>,
    /// IDs of the direct predecessors, ascending.
    pub depends_on: Vec<TaskId>,
}

/// One read of the queue for `graph`: the unfinished tasks in ID order and
/// the IDs of the claimable ones (`candidates`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GraphInput {
    pub tasks: Vec<GraphTask>,
    pub candidates: Vec<TaskId>,
}

/// An unfinished task as `graph` shows it (ADR-0023 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GraphNode {
    pub id: TaskId,
    pub status: TaskStatus,
    pub title: String,
    pub goal_id: Option<GoalId>,
    /// Status of the task's goal, present only for a task in a goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_status: Option<GoalStatus>,
    /// Every direct predecessor, ascending.
    pub depends_on: Vec<TaskId>,
    /// Unfinished tasks that depend on this one directly, ascending.
    pub blocks: Vec<TaskId>,
    /// How many unfinished tasks depend on this one directly or transitively:
    /// the tasks its completion moves closer to running.
    pub unblocks: usize,
    /// The direct predecessors that are still unfinished, ascending.
    pub ready_after: Vec<TaskId>,
}

/// The dependency view of the unfinished tasks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DependencyGraph {
    /// Unfinished tasks in ID order.
    pub tasks: Vec<GraphNode>,
    /// Claimable tasks in the order the supervisor claims them: most
    /// `unblocks` first, then ascending ID.
    pub candidates: Vec<TaskId>,
    /// The chain from the task with the most `unblocks` (lowest ID on a tie),
    /// each step to the directly blocked task with the most `unblocks`
    /// (lowest ID on a tie), down to a task that blocks nothing. Empty when
    /// no task blocks another.
    pub critical: Vec<TaskId>,
}

/// Compute the dependency view. Counts always span every unfinished task;
/// `goal_id` only narrows `tasks`, `candidates` and where `critical` starts
/// (the chain may then leave the goal). The queue rejects cycles, so the
/// dependencies form a DAG; a predecessor that is not in `input.tasks` is
/// finished.
pub fn dependency_graph(input: GraphInput, goal_id: Option<GoalId>) -> DependencyGraph {
    use std::collections::{BTreeMap, BTreeSet};
    let open: BTreeSet<TaskId> = input.tasks.iter().map(|task| task.id).collect();
    let mut blocks: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    for task in &input.tasks {
        for predecessor in &task.depends_on {
            if open.contains(predecessor) {
                blocks.entry(*predecessor).or_default().push(task.id);
            }
        }
    }
    for dependents in blocks.values_mut() {
        dependents.sort_unstable();
        dependents.dedup();
    }
    let direct = |id: TaskId| blocks.get(&id).map(Vec::as_slice).unwrap_or_default();
    let unblocks: BTreeMap<TaskId, usize> = open
        .iter()
        .map(|&id| {
            let mut reached = BTreeSet::new();
            let mut pending = direct(id).to_vec();
            while let Some(next) = pending.pop() {
                if reached.insert(next) {
                    pending.extend_from_slice(direct(next));
                }
            }
            (id, reached.len())
        })
        .collect();
    let count = |id: TaskId| unblocks.get(&id).copied().unwrap_or(0);
    // Most unblocks first, lowest ID on a tie.
    let rank = |id: &TaskId| (std::cmp::Reverse(count(*id)), *id);
    let in_goal = |task: &GraphTask| goal_id.is_none() || task.goal_id == goal_id;
    let goal_ids: BTreeSet<TaskId> = input
        .tasks
        .iter()
        .filter(|task| in_goal(task))
        .map(|task| task.id)
        .collect();
    let mut candidates: Vec<TaskId> = input
        .candidates
        .iter()
        .copied()
        .filter(|id| goal_id.is_none() || goal_ids.contains(id))
        .collect();
    candidates.sort_by_key(rank);
    let mut critical = Vec::new();
    let mut step = goal_ids.iter().copied().min_by_key(rank);
    if step.is_some_and(|id| count(id) == 0) {
        step = None;
    }
    while let Some(id) = step {
        critical.push(id);
        step = direct(id).iter().copied().min_by_key(rank);
    }
    let tasks = input
        .tasks
        .into_iter()
        .filter(|task| goal_ids.contains(&task.id))
        .map(|task| GraphNode {
            blocks: direct(task.id).to_vec(),
            unblocks: count(task.id),
            ready_after: task
                .depends_on
                .iter()
                .copied()
                .filter(|id| open.contains(id))
                .collect(),
            id: task.id,
            status: task.status,
            title: task.title,
            goal_id: task.goal_id,
            goal_status: task.goal_status,
            depends_on: task.depends_on,
        })
        .collect();
    DependencyGraph {
        tasks,
        candidates,
        critical,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[i64]) -> Vec<TaskId> {
        ids.iter().copied().map(TaskId::new).collect()
    }

    fn task(id: i64, goal_id: Option<i64>, depends_on: &[i64]) -> GraphTask {
        GraphTask {
            id: TaskId::new(id),
            status: if depends_on.is_empty() {
                TaskStatus::Ready
            } else {
                TaskStatus::Draft
            },
            title: format!("task {id}"),
            goal_id: goal_id.map(GoalId::new),
            goal_status: goal_id.map(|_| GoalStatus::Open),
            depends_on: ids(depends_on),
        }
    }

    /// 2 -> {3, 4} -> 5, 1 -> 6 (goal 1), and 7 waits only on the finished 9.
    fn input() -> GraphInput {
        GraphInput {
            tasks: vec![
                task(1, Some(1), &[]),
                task(2, None, &[]),
                task(3, None, &[2]),
                task(4, None, &[2]),
                task(5, None, &[3, 4]),
                task(6, Some(1), &[1]),
                task(7, None, &[9]),
            ],
            candidates: ids(&[1, 2, 7]),
        }
    }

    #[test]
    fn graph_counts_transitive_releases_and_follows_the_critical_chain() {
        let graph = dependency_graph(input(), None);
        let unblocks: Vec<(i64, usize)> = graph
            .tasks
            .iter()
            .map(|t| (t.id.as_i64(), t.unblocks))
            .collect();
        assert_eq!(
            unblocks,
            [(1, 1), (2, 3), (3, 1), (4, 1), (5, 0), (6, 0), (7, 0)]
        );
        assert_eq!(graph.tasks[1].blocks, ids(&[3, 4]));
        assert_eq!(graph.tasks[4].depends_on, ids(&[3, 4]));
        assert_eq!(graph.tasks[4].ready_after, ids(&[3, 4]));
        assert_eq!(graph.tasks[6].depends_on, ids(&[9]));
        assert!(graph.tasks[6].ready_after.is_empty());
        assert_eq!(graph.candidates, ids(&[2, 1, 7]));
        assert_eq!(graph.critical, ids(&[2, 3, 5]));
    }

    #[test]
    fn goal_narrows_tasks_candidates_and_the_critical_start() {
        let graph = dependency_graph(input(), Some(GoalId::new(1)));
        let tasks: Vec<TaskId> = graph.tasks.iter().map(|t| t.id).collect();
        assert_eq!(tasks, ids(&[1, 6]));
        assert_eq!(graph.candidates, ids(&[1]));
        assert_eq!(graph.critical, ids(&[1, 6]));
    }

    #[test]
    fn no_blocking_task_leaves_the_critical_chain_empty() {
        let graph = dependency_graph(
            GraphInput {
                tasks: vec![task(2, None, &[]), task(1, None, &[])],
                candidates: ids(&[2, 1]),
            },
            None,
        );
        assert_eq!(graph.candidates, ids(&[1, 2]));
        assert!(graph.critical.is_empty());
        assert!(
            dependency_graph(GraphInput::default(), None)
                .tasks
                .is_empty()
        );
    }
}

/// Seconds since the Unix epoch; zero before it.
pub fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// `time` as `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC; the epoch for a time before it.
pub fn timestamp(time: SystemTime) -> String {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = elapsed.as_secs() as i64;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        elapsed.subsec_millis()
    )
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Duration;

    struct At(SystemTime);

    impl Clock for At {
        fn system_time(&self) -> SystemTime {
            self.0
        }
    }

    #[test]
    fn a_clock_gives_unix_seconds_and_the_timestamp_column_form() {
        let clock = At(UNIX_EPOCH + Duration::from_millis(1_000_000_000_123));
        assert_eq!(clock.now(), 1_000_000_000);
        assert_eq!(clock.timestamp(), "2001-09-09T01:46:40.123Z");
        let before_epoch = At(UNIX_EPOCH - Duration::from_secs(1));
        assert_eq!(before_epoch.now(), 0);
        assert_eq!(before_epoch.timestamp(), "1970-01-01T00:00:00.000Z");
        // Leap days and the last millisecond of a year.
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29T00:00:00.000Z"
        );
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_millis(1_735_689_599_999)),
            "2024-12-31T23:59:59.999Z"
        );
    }
}

/// The last `max_bytes` of `text` at most, starting on a character boundary.
pub fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// `text` in a fenced block whose fence is longer than any backtick run in
/// it, labelled `info`.
pub fn fenced(info: &str, text: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let body = text.trim_end_matches('\n');
    if body.is_empty() {
        format!("{fence}{info}\n{fence}\n")
    } else {
        format!("{fence}{info}\n{body}\n{fence}\n")
    }
}

/// `text`, or `(none)` when it is blank.
pub fn or_none(text: &str) -> &str {
    if text.trim().is_empty() {
        "(none)"
    } else {
        text.trim_end()
    }
}

/// `path` as text; the runtime keeps every path it records as UTF-8.
pub fn path_text(path: &std::path::Path) -> anyhow::Result<String> {
    use anyhow::Context;
    path.to_str()
        .map(str::to_owned)
        .context("runtime paths must be valid UTF-8")
}
