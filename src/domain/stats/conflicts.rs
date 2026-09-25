//! `conflict_hotspots` of `stats` (goal 31, ADR-0044 decision 21): the
//! files the landings conflicted in, from the `conflicts` of each
//! `integration_deferred` (a rebase that stopped) and `conflict_precheck`
//! (`git merge-tree` before the session exits) in the window. Per file:
//! how many times it conflicted, in how many tasks, how many landings on
//! main changed it in the same window (from Git's history, read outside
//! the queue) and the share of those the conflicts are, and when it last
//! conflicted. A file main no longer has is told apart: renamed (with its
//! name now) or deleted. The threshold of the `conflict_hotspot` alert is
//! the `[conflicts]` table of `dagq.toml`.
//!
//! A rebase conflict of a run whose precheck already recorded the same
//! file against the same main is the same conflict and is counted once.
use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::Serialize;
use serde_json::Value;

use super::timestamp_millis;
use crate::domain::{EventId, RunEvent, TaskId};

/// Default least number of conflicts of a file that is an alert.
pub const DEFAULT_HOTSPOT_CONFLICTS: i64 = 3;
/// Default least share, in percent, of the landings that changed a file
/// that conflicted in it, for an alert.
pub const DEFAULT_HOTSPOT_RATIO_PERCENT: i64 = 20;

/// The thresholds of the `conflict_hotspot` alert, the `[conflicts]`
/// table of `dagq.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConflictConfig {
    pub hotspot_conflicts: i64,
    pub hotspot_ratio_percent: i64,
}

impl Default for ConflictConfig {
    fn default() -> Self {
        Self {
            hotspot_conflicts: DEFAULT_HOTSPOT_CONFLICTS,
            hotspot_ratio_percent: DEFAULT_HOTSPOT_RATIO_PERCENT,
        }
    }
}

impl ConflictConfig {
    /// The setting names of the `[conflicts]` table.
    pub const KEYS: [&str; 2] = ["hotspot_conflicts", "hotspot_ratio_percent"];

    /// The setting `key` set to `value`; `None` for a key the table does
    /// not have.
    pub fn set(&mut self, key: &str, value: i64) -> Option<()> {
        let field = match key {
            "hotspot_conflicts" => &mut self.hotspot_conflicts,
            "hotspot_ratio_percent" => &mut self.hotspot_ratio_percent,
            _ => return None,
        };
        *field = value;
        Some(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ConflictConfigReport {
    #[serde(flatten)]
    pub config: ConflictConfig,
    /// `file` (the `[conflicts]` of `dagq.toml`) or `default`.
    pub source: &'static str,
}

impl Default for ConflictConfigReport {
    fn default() -> Self {
        Self {
            config: ConflictConfig::default(),
            source: "default",
        }
    }
}

/// One path a commit on main touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainChange {
    /// The path after the commit (the new name of a rename).
    pub path: String,
    /// The old name of a rename.
    pub from: Option<String>,
    pub deleted: bool,
}

/// One commit on main's first-parent line: one landing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainCommit {
    /// Unix seconds of the commit.
    pub at: i64,
    pub changes: Vec<MainChange>,
}

/// Main's history since the earliest conflict, and the paths it has now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MainHistory {
    /// Oldest first.
    pub commits: Vec<MainCommit>,
    pub paths: HashSet<String>,
}

/// Main's history, or why it could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum History {
    Read(MainHistory),
    Unavailable(String),
}

impl Default for History {
    fn default() -> Self {
        Self::Unavailable("not read".to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HistoryCheck {
    /// Git's history of main was read: `landings` is the count of commits
    /// on main in the window.
    Checked { landings: i64 },
    /// It could not be; `landings`, `ratio` and `state` are unknown.
    Unavailable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConflictHotspots {
    /// Conflict events in the window (a rebase conflict repeating its
    /// precheck's is not counted again).
    pub count: i64,
    pub history: HistoryCheck,
    pub config: ConflictConfigReport,
    /// Most conflicts first.
    pub files: Vec<ConflictHotspot>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConflictHotspot {
    pub path: String,
    pub conflicts: i64,
    pub tasks: usize,
    pub task_ids: Vec<TaskId>,
    /// Commits on main in the window that changed the path (as it was
    /// named then); `None` without the history.
    pub landings: Option<i64>,
    /// `conflicts / landings`; `None` without landings.
    pub ratio: Option<f64>,
    pub last_conflict_at: String,
    /// `present` (main has the path), `renamed` (main has it under
    /// `renamed_to`), `deleted`, or `unknown` without the history.
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    /// Whether it crosses the `[conflicts]` thresholds; a file main no
    /// longer has never does.
    pub alert: bool,
}

struct Tally {
    conflicts: i64,
    tasks: BTreeSet<TaskId>,
    last: (EventId, String),
}

/// The conflicted files of `event`, when it is a conflict event.
fn conflicted(event: &RunEvent) -> Option<Vec<&str>> {
    if !matches!(
        event.kind.as_str(),
        "integration_deferred" | "conflict_precheck"
    ) {
        return None;
    }
    let files: Vec<&str> = event
        .payload
        .get("conflicts")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    (!files.is_empty()).then_some(files)
}

/// The unix millisecond of the earliest conflict event, for how far back
/// main's history is read; `None` when there is none.
pub fn earliest_conflict(events: &[RunEvent]) -> Option<i64> {
    events
        .iter()
        .filter(|event| conflicted(event).is_some())
        .filter_map(|event| timestamp_millis(&event.created_at))
        .min()
}

/// Aggregate the conflict events with `after < id <= upto` whose task
/// `counts` accepts, against `history`.
pub fn conflict_hotspots(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
    history: &History,
    config: ConflictConfigReport,
) -> ConflictHotspots {
    let window: Vec<&RunEvent> = events
        .iter()
        .filter(|event| event.id > after && event.id <= upto)
        .collect();
    // The window in unix milliseconds, from its first event to its last.
    let times: Vec<i64> = window
        .iter()
        .filter_map(|event| timestamp_millis(&event.created_at))
        .collect();
    let (start, end) = (times.iter().min().copied(), times.iter().max().copied());
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut tallies: BTreeMap<&str, Tally> = BTreeMap::new();
    let mut count = 0;
    for event in window.iter().filter(|event| counts(event.task_id)) {
        let Some(files) = conflicted(event) else {
            continue;
        };
        let run = event
            .run_id
            .as_ref()
            .map(|run| run.as_str().to_owned())
            .unwrap_or_default();
        let main = event.payload["main"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut counted = false;
        for file in files {
            if !seen.insert((run.clone(), main.clone(), file.to_owned())) && !main.is_empty() {
                continue;
            }
            counted = true;
            let tally = tallies.entry(file).or_insert_with(|| Tally {
                conflicts: 0,
                tasks: BTreeSet::new(),
                last: (event.id, event.created_at.clone()),
            });
            tally.conflicts += 1;
            tally.tasks.extend(event.task_id);
            tally.last = (event.id, event.created_at.clone());
        }
        if counted {
            count += 1;
        }
    }
    let in_window = |commit: &MainCommit| {
        let at = commit.at * 1000;
        start.is_some_and(|start| at >= start - 999) && end.is_some_and(|end| at <= end)
    };
    let check = match history {
        History::Read(history) => HistoryCheck::Checked {
            landings: history.commits.iter().filter(|c| in_window(c)).count() as i64,
        },
        History::Unavailable(reason) => HistoryCheck::Unavailable {
            reason: reason.clone(),
        },
    };
    let mut files: Vec<ConflictHotspot> = tallies
        .into_iter()
        .map(|(path, tally)| {
            let (landings, state, renamed_to) = match history {
                History::Read(history) => {
                    let landings = history
                        .commits
                        .iter()
                        .filter(|commit| in_window(commit))
                        .filter(|commit| {
                            commit.changes.iter().any(|change| {
                                change.path == path || change.from.as_deref() == Some(path)
                            })
                        })
                        .count() as i64;
                    let (state, renamed_to) = where_now(path, history, start);
                    (Some(landings), state, renamed_to)
                }
                History::Unavailable(_) => (None, "unknown", None),
            };
            #[allow(clippy::cast_precision_loss)]
            let ratio = landings
                .filter(|&landings| landings > 0)
                .map(|landings| tally.conflicts as f64 / landings as f64);
            let alert = matches!(state, "present" | "renamed" | "unknown")
                && tally.conflicts >= config.config.hotspot_conflicts
                && ratio.is_none_or(|ratio| {
                    ratio * 100.0 >= config.config.hotspot_ratio_percent as f64
                });
            ConflictHotspot {
                path: path.to_owned(),
                conflicts: tally.conflicts,
                tasks: tally.tasks.len(),
                task_ids: tally.tasks.into_iter().collect(),
                landings,
                ratio,
                last_conflict_at: tally.last.1,
                state,
                renamed_to,
                alert,
            }
        })
        .collect();
    files.sort_by(|a, b| {
        b.conflicts
            .cmp(&a.conflicts)
            .then(b.tasks.cmp(&a.tasks))
            .then(a.path.cmp(&b.path))
    });
    ConflictHotspots {
        count,
        history: check,
        config,
        files,
    }
}

/// Where `path` is on main now: `present`, `renamed` (following the renames
/// of the commits since `start`) or `deleted`.
fn where_now(
    path: &str,
    history: &MainHistory,
    start: Option<i64>,
) -> (&'static str, Option<String>) {
    if history.paths.contains(path) {
        return ("present", None);
    }
    let mut name = path.to_owned();
    for commit in history
        .commits
        .iter()
        .filter(|commit| start.is_none_or(|start| commit.at * 1000 >= start - 999))
    {
        if let Some(change) = commit
            .changes
            .iter()
            .find(|change| change.from.as_deref() == Some(name.as_str()))
        {
            name.clone_from(&change.path);
        }
    }
    if name != path && history.paths.contains(&name) {
        ("renamed", Some(name))
    } else {
        ("deleted", None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RunId;
    use serde_json::json;

    const R1: &str = "11111111-1111-4111-8111-111111111111";
    const R2: &str = "22222222-2222-4222-8222-222222222222";
    const T: i64 = 1_800_000_000;

    fn at(secs: i64) -> String {
        crate::application::timestamp(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64),
        )
    }

    fn event(id: i64, run: &str, task: i64, kind: &str, payload: Value, secs: i64) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: Some(RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: at(secs),
        }
    }

    fn change(path: &str) -> MainChange {
        MainChange {
            path: path.to_owned(),
            from: None,
            deleted: false,
        }
    }

    fn commit(secs: i64, changes: Vec<MainChange>) -> MainCommit {
        MainCommit { at: secs, changes }
    }

    fn events() -> Vec<RunEvent> {
        vec![
            event(1, R1, 1, "run_claimed", json!({}), T),
            // A precheck, then the rebase conflict it repeats against the
            // same main: one conflict of a.rs.
            event(
                2,
                R1,
                1,
                "conflict_precheck",
                json!({"main": "m1", "conflicts": ["a.rs", "gone.rs"]}),
                T + 10,
            ),
            event(
                3,
                R1,
                1,
                "integration_deferred",
                json!({"main": "m1", "conflicts": ["a.rs"], "reason": "rebase onto main m1 conflicted in a.rs"}),
                T + 20,
            ),
            // Against a newer main it is a second conflict.
            event(
                4,
                R1,
                1,
                "integration_deferred",
                json!({"main": "m2", "conflicts": ["a.rs", "old.rs"]}),
                T + 30,
            ),
            event(
                5,
                R2,
                2,
                "conflict_precheck",
                json!({"main": "m2", "conflicts": ["a.rs"]}),
                T + 40,
            ),
            // Not a conflict: no files.
            event(
                6,
                R2,
                2,
                "integration_deferred",
                json!({"code": "verification_failed"}),
                T + 50,
            ),
        ]
    }

    fn history() -> History {
        History::Read(MainHistory {
            commits: vec![
                // Before the window: not a landing of it.
                commit(T - 100, vec![change("a.rs")]),
                commit(T + 5, vec![change("a.rs"), change("old.rs")]),
                commit(T + 25, vec![change("a.rs")]),
                commit(T + 35, vec![change("a.rs"), change("gone.rs")]),
                commit(T + 45, vec![change("a.rs")]),
                commit(
                    T + 48,
                    vec![
                        MainChange {
                            path: "new.rs".into(),
                            from: Some("old.rs".into()),
                            deleted: false,
                        },
                        MainChange {
                            path: "gone.rs".into(),
                            from: None,
                            deleted: true,
                        },
                    ],
                ),
            ],
            paths: ["a.rs", "new.rs"].into_iter().map(str::to_owned).collect(),
        })
    }

    #[test]
    fn files_are_counted_with_their_tasks_landings_ratio_and_where_they_are_now() {
        let hotspots = conflict_hotspots(
            &events(),
            EventId::new(0),
            EventId::new(6),
            |_| true,
            &history(),
            ConflictConfigReport::default(),
        );
        assert_eq!(hotspots.count, 3);
        assert_eq!(hotspots.history, HistoryCheck::Checked { landings: 5 });
        let paths: Vec<_> = hotspots.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "gone.rs", "old.rs"]);
        let a = &hotspots.files[0];
        assert_eq!((a.conflicts, a.tasks), (3, 2));
        assert_eq!(a.task_ids, [TaskId::new(1), TaskId::new(2)]);
        assert_eq!(a.landings, Some(4));
        assert_eq!(a.ratio, Some(0.75));
        assert_eq!(a.last_conflict_at, at(T + 40));
        assert_eq!((a.state, a.renamed_to.as_deref()), ("present", None));
        assert!(a.alert);
        let gone = &hotspots.files[1];
        assert_eq!((gone.state, gone.alert), ("deleted", false));
        assert_eq!(gone.landings, Some(2));
        let old = &hotspots.files[2];
        assert_eq!(
            (old.state, old.renamed_to.as_deref()),
            ("renamed", Some("new.rs"))
        );
        assert_eq!((old.conflicts, old.landings), (1, Some(2)));
        assert!(!old.alert, "under the conflicts threshold");
        let json = serde_json::to_value(&hotspots).unwrap();
        assert_eq!(json["history"]["status"], "checked");
        assert_eq!(json["config"]["hotspot_conflicts"], 3);
        assert_eq!(json["config"]["source"], "default");
        assert!(json["files"][0].get("renamed_to").is_none());
    }

    #[test]
    fn the_thresholds_the_window_and_the_task_filter_apply() {
        // A ratio under the threshold is not an alert.
        let strict = ConflictConfigReport {
            config: ConflictConfig {
                hotspot_conflicts: 3,
                hotspot_ratio_percent: 80,
            },
            source: "file",
        };
        let hotspots = conflict_hotspots(
            &events(),
            EventId::new(0),
            EventId::new(6),
            |_| true,
            &history(),
            strict,
        );
        assert!(!hotspots.files[0].alert);
        // Past event 3: the first conflict of a.rs is out of the window.
        let later = conflict_hotspots(
            &events(),
            EventId::new(3),
            EventId::new(6),
            |_| true,
            &history(),
            ConflictConfigReport::default(),
        );
        assert_eq!(later.files[0].conflicts, 2);
        // Only task 2's.
        let task_two = conflict_hotspots(
            &events(),
            EventId::new(0),
            EventId::new(6),
            |task| task == Some(TaskId::new(2)),
            &history(),
            ConflictConfigReport::default(),
        );
        assert_eq!(task_two.count, 1);
        assert_eq!(task_two.files.len(), 1);
    }

    #[test]
    fn without_the_history_the_counts_stand_alone() {
        let hotspots = conflict_hotspots(
            &events(),
            EventId::new(0),
            EventId::new(6),
            |_| true,
            &History::default(),
            ConflictConfigReport::default(),
        );
        assert_eq!(
            hotspots.history,
            HistoryCheck::Unavailable {
                reason: "not read".into()
            }
        );
        let a = &hotspots.files[0];
        assert_eq!((a.landings, a.ratio, a.state), (None, None, "unknown"));
        assert!(a.alert, "judged on the count alone");
        assert_eq!(earliest_conflict(&events()), Some((T + 10) * 1000));
        assert_eq!(earliest_conflict(&[]), None);
    }

    #[test]
    fn the_config_sets_its_keys_only() {
        let mut config = ConflictConfig::default();
        assert_eq!(config.set("hotspot_conflicts", 5), Some(()));
        assert_eq!(config.set("hotspot_ratio_percent", 50), Some(()));
        assert_eq!(config.set("other", 1), None);
        assert_eq!(
            config,
            ConflictConfig {
                hotspot_conflicts: 5,
                hotspot_ratio_percent: 50
            }
        );
    }
}
