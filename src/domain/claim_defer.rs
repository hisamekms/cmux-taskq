//! Deferring the claim of one task (ADR-0069): a candidate whose expected
//! files meet those of a run in flight on a file the landings keep
//! conflicting in (a hotspot of `stats`' `conflict_hotspots`) is passed
//! over in that pass, and the next candidate is claimed instead. Where
//! [`super::claim_hold`] holds every claim for the queue, this defers one
//! task; the two share the way they are recorded and shown: a task event
//! when the deferral starts (`claim_deferred`) and one when it ends
//! (`claim_deferral_ended`), which `status` and `stats` read.
//!
//! A task of interrupt priority is never deferred, and a deferral ends
//! once it has lasted `[conflicts] defer_max_secs`: the task is claimed
//! then even if the files still meet.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::{Value, json};

use super::{EventId, RunEvent, TaskId, scope::glob_matches, stats::timestamp_millis};

/// Recorded on the task when its claim is first deferred (`reason`,
/// `files`, `runs`, `max_secs`, `message`, `supervisor`).
pub const CLAIM_DEFERRED: &str = "claim_deferred";
/// Recorded on the task when its deferral ends (`reason`, `why`,
/// `deferred_secs`, `supervisor`): `cleared` (the files no longer meet or
/// the task became an interrupt), `expired` (it lasted the limit) or
/// `not_candidate` (it left the candidates: claimed elsewhere, canceled,
/// blocked again).
pub const CLAIM_DEFERRAL_ENDED: &str = "claim_deferral_ended";
/// The kinds that say where a task's deferral stands; a claim of the task
/// ends whatever came before it.
pub const DEFERRAL_KINDS: [&str; 3] = [CLAIM_DEFERRED, CLAIM_DEFERRAL_ENDED, "run_claimed"];

/// The only reason so far: the files meet on a hotspot.
pub const HOT_FILES: &str = "hot_files";

/// The default of `[conflicts] defer_max_secs`: a deferral lasts at most an
/// hour. The runs of this queue work for 24 minutes at the median
/// (`dagq stats` of 2026-09-26), so an hour lets the run in the way work
/// and mostly land, while the task deferred never waits longer than about
/// two of them.
pub const DEFAULT_DEFER_MAX_SECS: i64 = 3600;

/// How many related landed tasks give the files of a task that declares no
/// paths.
pub const RELATED_TASKS: usize = 3;

/// The files a run in flight is expected to touch: its diff from its base
/// and the expected files of its task, as paths and globs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub run_id: String,
    pub task_id: TaskId,
    pub files: Vec<String>,
}

/// Where a candidate's expected files meet the runs in flight on hotspots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overlap {
    /// The hotspots both sides touch, in the order of `hot`.
    pub files: Vec<String>,
    /// The runs in flight that touch them.
    pub runs: Vec<OverlapRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct OverlapRun {
    pub run_id: String,
    pub task_id: TaskId,
}

/// Whether `files` (paths or `--paths` globs) touch `path`.
pub fn touches(files: &[String], path: &str) -> bool {
    files.iter().any(|file| glob_matches(file, path))
}

/// The expected files of a task: its declared paths, or, when it declares
/// none, the files the landings of its most related tasks changed.
pub fn expected_files(declared: &[String], related_changes: &[String]) -> Vec<String> {
    let files = if declared.is_empty() {
        related_changes
    } else {
        declared
    };
    let mut unique: Vec<String> = Vec::new();
    for file in files {
        if !unique.contains(file) {
            unique.push(file.clone());
        }
    }
    unique
}

/// Where `candidate` (its expected files) meets `in_flight` on the files of
/// `hot`; `None` when it does not.
pub fn overlap(hot: &[String], candidate: &[String], in_flight: &[InFlight]) -> Option<Overlap> {
    let mut files = Vec::new();
    let mut runs = BTreeSet::new();
    for path in hot.iter().filter(|path| touches(candidate, path)) {
        let meeting: Vec<&InFlight> = in_flight
            .iter()
            .filter(|run| touches(&run.files, path))
            .collect();
        if meeting.is_empty() {
            continue;
        }
        files.push(path.clone());
        runs.extend(meeting.into_iter().map(|run| OverlapRun {
            run_id: run.run_id.clone(),
            task_id: run.task_id,
        }));
    }
    (!files.is_empty()).then(|| Overlap {
        files,
        runs: runs.into_iter().collect(),
    })
}

/// A task's deferral as the supervisor keeps it: since when (unix
/// seconds), and whether it ran out, which lets the task be claimed while
/// the files still meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferral {
    pub since: i64,
    pub expired: bool,
}

/// The deferrals in place from each task's latest [`DEFERRAL_KINDS`]
/// event: an open `claim_deferred`, or a deferral that expired and whose
/// task was not claimed since. What a supervisor starts from, so the limit
/// holds across restarts.
pub fn deferrals_in_place(latest: &[RunEvent]) -> HashMap<TaskId, Deferral> {
    latest
        .iter()
        .filter_map(|event| {
            let task = event.task_id?;
            let since = || timestamp_millis(&event.created_at).map(|ms| ms / 1000);
            match event.kind.as_str() {
                CLAIM_DEFERRED => Some((
                    task,
                    Deferral {
                        since: since()?,
                        expired: false,
                    },
                )),
                CLAIM_DEFERRAL_ENDED if text(event, "why") == Some("expired") => Some((
                    task,
                    Deferral {
                        since: since()?,
                        expired: true,
                    },
                )),
                _ => None,
            }
        })
        .collect()
}

/// What to do with one candidate this pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Claim it (or pass it on to the claim); `event` ends a deferral.
    Claim {
        event: Option<(&'static str, Value)>,
    },
    /// Pass over it; `event` starts the deferral.
    Defer {
        event: Option<(&'static str, Value)>,
    },
}

/// Judge one candidate: `interrupt` for a task of interrupt priority,
/// `overlap` of its files, `deferral` the one in place (updated here),
/// `now` in unix seconds.
pub fn decide(
    interrupt: bool,
    overlap: Option<Overlap>,
    deferral: &mut Option<Deferral>,
    now: i64,
    max_secs: i64,
    token: &str,
) -> Decision {
    let ended = |why: &str, since: i64| {
        Some((
            CLAIM_DEFERRAL_ENDED,
            json!({
                "reason": HOT_FILES,
                "why": why,
                "deferred_secs": (now - since).max(0),
                "supervisor": token,
            }),
        ))
    };
    match (overlap.filter(|_| !interrupt), *deferral) {
        (Some(_), Some(open)) if open.expired => Decision::Claim { event: None },
        (Some(_), Some(open)) if now - open.since >= max_secs => {
            *deferral = Some(Deferral {
                since: open.since,
                expired: true,
            });
            Decision::Claim {
                event: ended("expired", open.since),
            }
        }
        (Some(_), Some(_)) => Decision::Defer { event: None },
        // No limit left to wait for: nothing is deferred.
        (Some(_), None) if max_secs <= 0 => Decision::Claim { event: None },
        (Some(overlap), None) => {
            *deferral = Some(Deferral {
                since: now,
                expired: false,
            });
            let message = message(&overlap, max_secs);
            Decision::Defer {
                event: Some((
                    CLAIM_DEFERRED,
                    json!({
                        "reason": HOT_FILES,
                        "files": overlap.files,
                        "runs": overlap.runs,
                        "max_secs": max_secs,
                        "message": message,
                        "supervisor": token,
                    }),
                )),
            }
        }
        // An expired deferral stays expired until the task leaves the
        // candidates (it is claimed), so it is not deferred anew.
        (None, Some(open)) if open.expired => Decision::Claim { event: None },
        (None, Some(open)) => {
            *deferral = None;
            Decision::Claim {
                event: ended("cleared", open.since),
            }
        }
        (None, None) => Decision::Claim { event: None },
    }
}

/// The event that ends `deferral` of a task that left the candidates, if
/// it was still open.
pub fn left(deferral: Deferral, now: i64, token: &str) -> Option<(&'static str, Value)> {
    (!deferral.expired).then(|| {
        (
            CLAIM_DEFERRAL_ENDED,
            json!({
                "reason": HOT_FILES,
                "why": "not_candidate",
                "deferred_secs": (now - deferral.since).max(0),
                "supervisor": token,
            }),
        )
    })
}

/// Why the claim is deferred, for the log and the event.
pub fn message(overlap: &Overlap, max_secs: i64) -> String {
    let runs: Vec<String> = overlap
        .runs
        .iter()
        .map(|run| format!("task {} (run {})", run.task_id, run.run_id))
        .collect();
    format!(
        "the task's files meet those of {} on the conflict hotspots {}: not claimed until they land or for {max_secs} seconds",
        runs.join(", "),
        overlap.files.join(", ")
    )
}

/// A deferral in progress, for `status` and `stats`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenDeferral {
    pub task_id: TaskId,
    pub reason: String,
    pub since: String,
    pub files: Value,
    pub runs: Value,
    pub supervisor: Option<String>,
}

impl OpenDeferral {
    pub fn of(event: &RunEvent) -> Option<Self> {
        (event.kind == CLAIM_DEFERRED).then(|| Self {
            task_id: event.task_id.unwrap_or(TaskId::new(0)),
            reason: text(event, "reason").unwrap_or("unknown").to_owned(),
            since: event.created_at.clone(),
            files: event.payload.get("files").cloned().unwrap_or(Value::Null),
            runs: event.payload.get("runs").cloned().unwrap_or(Value::Null),
            supervisor: text(event, "supervisor").map(str::to_owned),
        })
    }
}

/// One way deferrals ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WhyEnded {
    pub count: i64,
    pub secs: i64,
}

/// The deferred claims (`stats`' `claim_deferrals`): those that started in
/// the window, the seconds they lasted (one still open lasts to the
/// window's end), how they ended, the hotspots they were deferred on, and
/// the deferrals in progress now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClaimDeferrals {
    pub count: i64,
    pub secs: i64,
    pub by_end: BTreeMap<String, WhyEnded>,
    pub by_file: BTreeMap<String, i64>,
    pub deferred: Vec<OpenDeferral>,
}

/// Aggregate the deferrals of `events` (ascending id) that started with
/// `after < id <= upto` on a task `counts` accepts; `end_ms` ends one still
/// open. A deferral ends at the next `claim_deferral_ended` of its task or
/// at its `run_claimed`; `deferred` is the queue's now either way.
pub fn claim_deferrals(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimDeferrals {
    let mut stats = ClaimDeferrals::default();
    let mut open: BTreeMap<TaskId, (&RunEvent, i64)> = BTreeMap::new();
    let close = |stats: &mut ClaimDeferrals, (event, start): (&RunEvent, i64), end, why: &str| {
        if event.id <= after || event.id > upto || !counts(event.task_id) {
            return;
        }
        let secs = (i64::min(end, end_ms) - start).max(0) / 1000;
        stats.count += 1;
        stats.secs += secs;
        let entry = stats.by_end.entry(why.to_owned()).or_default();
        entry.count += 1;
        entry.secs += secs;
        for file in event
            .payload
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            *stats.by_file.entry(file.to_owned()).or_default() += 1;
        }
    };
    for event in events {
        let Some(task) = event.task_id else {
            continue;
        };
        let at = || timestamp_millis(&event.created_at).unwrap_or(end_ms);
        match event.kind.as_str() {
            CLAIM_DEFERRED => {
                if let Some(held) = open.remove(&task) {
                    close(&mut stats, held, at(), "superseded");
                }
                open.insert(task, (event, at()));
            }
            CLAIM_DEFERRAL_ENDED => {
                if let Some(held) = open.remove(&task) {
                    close(
                        &mut stats,
                        held,
                        at(),
                        text(event, "why").unwrap_or("unknown"),
                    );
                }
            }
            "run_claimed" => {
                if let Some(held) = open.remove(&task) {
                    close(&mut stats, held, at(), "claimed");
                }
            }
            _ => {}
        }
    }
    for (_, held) in open {
        stats.deferred.extend(OpenDeferral::of(held.0));
        close(&mut stats, held, end_ms, "open");
    }
    stats
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, task: i64, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-26T01:{at}.000Z"),
        }
    }

    fn run(id: &str, task: i64, files: &[&str]) -> InFlight {
        InFlight {
            run_id: id.to_owned(),
            task_id: TaskId::new(task),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn files_meet_only_on_a_hotspot_both_sides_touch() {
        let hot = strings(&["docs/plans/current.md", "src/schema.rs"]);
        let in_flight = [
            run("r1", 1, &["docs/**", "src/a.rs"]),
            run("r2", 2, &["src/schema.rs"]),
        ];
        let found = overlap(&hot, &strings(&["docs/plans/*.md"]), &in_flight).unwrap();
        assert_eq!(found.files, strings(&["docs/plans/current.md"]));
        assert_eq!(
            found.runs,
            vec![OverlapRun {
                run_id: "r1".into(),
                task_id: TaskId::new(1)
            }]
        );
        let both = overlap(&hot, &strings(&["**"]), &in_flight).unwrap();
        assert_eq!(both.files.len(), 2);
        assert_eq!(both.runs.len(), 2);
        // A file both touch that is no hotspot, or a hotspot only one side
        // touches, does not defer.
        assert_eq!(overlap(&hot, &strings(&["src/a.rs"]), &in_flight), None);
        assert_eq!(overlap(&hot, &strings(&["src/schema.rs"]), &[]), None);
        assert_eq!(overlap(&[], &strings(&["**"]), &in_flight), None);
        assert_eq!(overlap(&hot, &[], &in_flight), None);
    }

    #[test]
    fn declared_paths_come_before_the_related_landings() {
        assert_eq!(
            expected_files(&strings(&["docs/**"]), &strings(&["src/a.rs"])),
            strings(&["docs/**"])
        );
        assert_eq!(
            expected_files(&[], &strings(&["src/a.rs", "src/b.rs", "src/a.rs"])),
            strings(&["src/a.rs", "src/b.rs"])
        );
    }

    fn hot() -> Option<Overlap> {
        Some(Overlap {
            files: strings(&["x.md"]),
            runs: vec![OverlapRun {
                run_id: "r1".into(),
                task_id: TaskId::new(1),
            }],
        })
    }

    #[test]
    fn a_deferral_starts_holds_and_ends_once() {
        let mut deferral = None;
        let Decision::Defer {
            event: Some((kind, payload)),
        } = decide(false, hot(), &mut deferral, 100, 60, "s")
        else {
            panic!("deferred with its event")
        };
        assert_eq!(kind, CLAIM_DEFERRED);
        assert_eq!(payload["reason"], "hot_files");
        assert_eq!(payload["files"], json!(["x.md"]));
        assert_eq!(payload["runs"][0]["run_id"], "r1");
        assert_eq!(payload["max_secs"], 60);
        assert!(payload["message"].as_str().unwrap().contains("task 1"));
        assert_eq!(
            deferral,
            Some(Deferral {
                since: 100,
                expired: false
            })
        );
        // Still meeting: deferred again, not recorded again.
        assert_eq!(
            decide(false, hot(), &mut deferral, 130, 60, "s"),
            Decision::Defer { event: None }
        );
        // The files no longer meet: claimed, and the deferral ends.
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(false, None, &mut deferral, 150, 60, "s")
        else {
            panic!("claimed with the end")
        };
        assert_eq!(kind, CLAIM_DEFERRAL_ENDED);
        assert_eq!(payload["why"], "cleared");
        assert_eq!(payload["deferred_secs"], 50);
        assert_eq!(deferral, None);
        assert_eq!(
            decide(false, None, &mut deferral, 160, 60, "s"),
            Decision::Claim { event: None }
        );
    }

    #[test]
    fn an_interrupt_is_never_deferred_and_ends_a_deferral() {
        let mut deferral = None;
        assert_eq!(
            decide(true, hot(), &mut deferral, 100, 60, "s"),
            Decision::Claim { event: None }
        );
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((_, payload)),
        } = decide(true, hot(), &mut deferral, 110, 60, "s")
        else {
            panic!("an interrupt is claimed")
        };
        assert_eq!(payload["why"], "cleared");
    }

    #[test]
    fn a_deferral_past_its_limit_is_claimed_and_not_deferred_again() {
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(false, hot(), &mut deferral, 160, 60, "s")
        else {
            panic!("expired")
        };
        assert_eq!(kind, CLAIM_DEFERRAL_ENDED);
        assert_eq!(payload["why"], "expired");
        assert_eq!(payload["deferred_secs"], 60);
        assert!(deferral.unwrap().expired);
        // Not claimed in that pass (no slot): the next one claims it, and
        // records nothing more.
        assert_eq!(
            decide(false, hot(), &mut deferral, 170, 60, "s"),
            Decision::Claim { event: None }
        );
        // The files stop meeting and meet again: still expired, not
        // deferred anew until the task is claimed.
        assert_eq!(
            decide(false, None, &mut deferral, 180, 60, "s"),
            Decision::Claim { event: None }
        );
        assert!(deferral.unwrap().expired);
        assert_eq!(
            decide(false, hot(), &mut deferral, 190, 60, "s"),
            Decision::Claim { event: None }
        );
        // A limit of 0 defers nothing.
        let mut none = None;
        assert_eq!(
            decide(false, hot(), &mut none, 100, 0, "s"),
            Decision::Claim { event: None }
        );
        assert_eq!(none, None);
    }

    #[test]
    fn a_task_that_leaves_the_candidates_ends_its_open_deferral() {
        let open = Deferral {
            since: 100,
            expired: false,
        };
        let (kind, payload) = left(open, 130, "s").unwrap();
        assert_eq!(kind, CLAIM_DEFERRAL_ENDED);
        assert_eq!(payload["why"], "not_candidate");
        assert_eq!(payload["deferred_secs"], 30);
        assert_eq!(
            left(
                Deferral {
                    expired: true,
                    ..open
                },
                130,
                "s"
            ),
            None
        );
    }

    #[test]
    fn the_deferrals_in_place_come_from_each_tasks_latest_event() {
        let latest = [
            event(1, 1, CLAIM_DEFERRED, json!({}), "00:10"),
            event(
                2,
                2,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "expired"}),
                "00:20",
            ),
            event(
                3,
                3,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "cleared"}),
                "00:30",
            ),
            event(4, 4, "run_claimed", json!({}), "00:40"),
        ];
        let place = deferrals_in_place(&latest);
        assert_eq!(place.len(), 2);
        let first = place[&TaskId::new(1)];
        assert!(!first.expired);
        assert_eq!(
            first.since,
            timestamp_millis("2026-09-26T01:00:10.000Z").unwrap() / 1000
        );
        assert!(place[&TaskId::new(2)].expired);
    }

    #[test]
    fn stats_count_the_deferrals_by_how_they_ended_and_show_the_open_ones() {
        let deferred = |id, task, at| {
            event(
                id,
                task,
                CLAIM_DEFERRED,
                json!({"reason": "hot_files", "files": ["x.md"], "runs": [], "supervisor": "s"}),
                at,
            )
        };
        let events = [
            deferred(1, 1, "00:00"),
            deferred(2, 2, "00:00"),
            deferred(3, 3, "00:00"),
            event(
                4,
                1,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "cleared"}),
                "00:30",
            ),
            event(5, 2, "run_claimed", json!({}), "00:40"),
            event(6, 9, "run_claimed", json!({}), "00:40"),
        ];
        let end = timestamp_millis("2026-09-26T01:01:00.000Z").unwrap();
        let stats = claim_deferrals(&events, EventId::new(0), EventId::new(6), end, |_| true);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.secs, 30 + 40 + 60);
        assert_eq!(stats.by_end["cleared"], WhyEnded { count: 1, secs: 30 });
        assert_eq!(stats.by_end["claimed"], WhyEnded { count: 1, secs: 40 });
        assert_eq!(stats.by_end["open"], WhyEnded { count: 1, secs: 60 });
        assert_eq!(stats.by_file["x.md"], 3);
        assert_eq!(stats.deferred.len(), 1);
        assert_eq!(stats.deferred[0].task_id, TaskId::new(3));
        assert_eq!(stats.deferred[0].files, json!(["x.md"]));
        assert_eq!(stats.deferred[0].supervisor.as_deref(), Some("s"));
        // Out of the window or of the goal: not counted, still shown.
        let later = claim_deferrals(&events, EventId::new(3), EventId::new(6), end, |_| true);
        assert_eq!(later.count, 0);
        assert_eq!(later.deferred.len(), 1);
        let none = claim_deferrals(&events, EventId::new(0), EventId::new(6), end, |_| false);
        assert_eq!(none.count, 0);
        // A deferral recorded again replaces the one before.
        let again = [deferred(1, 1, "00:00"), deferred(2, 1, "00:10")];
        let stats = claim_deferrals(&again, EventId::new(0), EventId::new(2), end, |_| true);
        assert_eq!(stats.by_end["superseded"].count, 1);
        assert_eq!(stats.deferred.len(), 1);
    }
}
