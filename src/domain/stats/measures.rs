//! What each run was measured under (goal 21, task 197), as `stats` reads
//! it from the events: the versions and load `run_claimed` recorded, the
//! load over its work (`receipt_observed`), its validation
//! (`validation_finished`) and its verification commands
//! (`verification_command` of `integrate`); and the aggregates over them:
//! the runs per version and per load band, and the time each verification
//! command takes.
use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::{Intervals, RunStats, intervals, median_f64};
use crate::domain::{
    EventId, RunEvent, TaskId,
    measure::{load_band, load_band_order},
};

/// The load average over one interval of a run, and its band (by the mean).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct IntervalLoad {
    pub mean: Option<f64>,
    pub max: Option<f64>,
    pub band: Option<&'static str>,
}

impl IntervalLoad {
    fn new(mean: Option<f64>, max: Option<f64>) -> Option<Self> {
        (mean.is_some() || max.is_some()).then(|| Self {
            mean,
            max,
            band: mean.map(load_band),
        })
    }

    fn of(payload: &Value) -> Option<Self> {
        Self::new(
            payload.get("load_avg_mean").and_then(Value::as_f64),
            payload.get("load_avg_max").and_then(Value::as_f64),
        )
    }
}

/// The load over a run's intervals: `work` (to its first receipt),
/// `validate` (to its first validation) and `verify` (its `integrate`
/// verification commands, the mean weighted by their durations); null for
/// an interval recorded without it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunLoad {
    pub work: Option<IntervalLoad>,
    pub validate: Option<IntervalLoad>,
    pub verify: Option<IntervalLoad>,
}

/// One run's measures: what its `run_claimed` recorded (null for a run
/// claimed by hand, or before they were recorded), the load over its
/// intervals and its `load_band`: the band of its work's mean load, or of
/// the load at its claim when that is all there is.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RunMeasures {
    pub dagq_version: Option<String>,
    pub claude_version: Option<String>,
    pub rustc_release: Option<String>,
    pub rustc_host: Option<String>,
    pub claim_parallel: Option<i64>,
    pub claim_slots: Option<i64>,
    pub claim_load_avg: Option<f64>,
    pub load: RunLoad,
    pub load_band: Option<&'static str>,
}

/// The measures of one run as its events come in.
#[derive(Debug, Default)]
pub(super) struct MeasureTrack {
    measures: RunMeasures,
    claimed: bool,
    /// The first `receipt_observed` / `validation_finished` was seen: a
    /// later one's load belongs to another interval.
    receipt_seen: bool,
    validated_seen: bool,
    verify_sum: f64,
    verify_weight: f64,
    verify_max: Option<f64>,
}

impl MeasureTrack {
    pub(super) fn observe(&mut self, event: &RunEvent) {
        let payload = &event.payload;
        let text = |key: &str| payload.get(key).and_then(Value::as_str).map(str::to_owned);
        let measures = &mut self.measures;
        match event.kind.as_str() {
            "run_claimed" if !self.claimed => {
                self.claimed = true;
                measures.dagq_version = text("dagq_version");
                measures.claude_version = text("claude_version");
                measures.rustc_release = text("rustc_release");
                measures.rustc_host = text("rustc_host");
                measures.claim_parallel = payload.get("parallel").and_then(Value::as_i64);
                measures.claim_slots = payload.get("slots").and_then(Value::as_i64);
                measures.claim_load_avg = payload.get("load_avg").and_then(Value::as_f64);
            }
            "receipt_observed" if !self.receipt_seen => {
                self.receipt_seen = true;
                measures.load.work = IntervalLoad::of(payload);
            }
            "validation_finished" if !self.validated_seen => {
                self.validated_seen = true;
                measures.load.validate = IntervalLoad::of(payload);
            }
            "verification_command" if payload["phase"] == "integration" => {
                let Some(load) = IntervalLoad::of(payload) else {
                    return;
                };
                if let Some(mean) = load.mean {
                    // A command recorded without its duration counts as one second.
                    let weight = payload
                        .get("duration_secs")
                        .and_then(Value::as_f64)
                        .filter(|secs| *secs > 0.0)
                        .unwrap_or(1.0);
                    self.verify_sum += mean * weight;
                    self.verify_weight += weight;
                }
                if let Some(max) = load.max {
                    self.verify_max = Some(self.verify_max.map_or(max, |m| m.max(max)));
                }
            }
            _ => {}
        }
    }

    pub(super) fn finish(mut self) -> RunMeasures {
        let mean = (self.verify_weight > 0.0)
            .then(|| (self.verify_sum / self.verify_weight * 100.0).round() / 100.0);
        self.measures.load.verify = IntervalLoad::new(mean, self.verify_max);
        self.measures.load_band = self
            .measures
            .load
            .work
            .as_ref()
            .and_then(|work| work.band)
            .or_else(|| self.measures.claim_load_avg.map(load_band));
        self.measures
    }
}

/// The runs of one version (or load band): null for the runs without one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionStats {
    pub version: Option<String>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

/// The runs grouped by what they were claimed with (task 197): the build
/// identifier of `dagq`, Claude Code's version, and the host's `rustc`
/// (`<release> <host>`), each by name with the runs without one last.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Versions {
    pub dagq: Vec<VersionStats>,
    pub claude: Vec<VersionStats>,
    pub rustc: Vec<VersionStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LoadBandStats {
    pub band: Option<&'static str>,
    #[serde(flatten)]
    pub intervals: Intervals,
}

fn grouped<K: Ord + Clone>(
    runs: &[&RunStats],
    key: impl Fn(&RunStats) -> Option<K>,
) -> Vec<(Option<K>, Intervals)> {
    let mut groups: BTreeMap<(bool, Option<K>), Vec<&RunStats>> = BTreeMap::new();
    for run in runs {
        let key = key(run);
        groups.entry((key.is_none(), key)).or_default().push(run);
    }
    groups
        .into_iter()
        .map(|((_, key), runs)| (key, intervals(&runs)))
        .collect()
}

pub(super) fn versions(runs: &[&RunStats]) -> Versions {
    let by = |key: fn(&RunStats) -> Option<String>| {
        grouped(runs, key)
            .into_iter()
            .map(|(version, intervals)| VersionStats { version, intervals })
            .collect()
    };
    Versions {
        dagq: by(|run| run.measures.dagq_version.clone()),
        claude: by(|run| run.measures.claude_version.clone()),
        rustc: by(|run| {
            let measures = &run.measures;
            match (&measures.rustc_release, &measures.rustc_host) {
                (None, None) => None,
                (release, host) => Some(format!(
                    "{} {}",
                    release.as_deref().unwrap_or("unknown"),
                    host.as_deref().unwrap_or("unknown")
                )),
            }
        }),
    }
}

/// The runs per `load_band`, lightest first, the runs without one last.
pub(super) fn load_bands(runs: &[&RunStats]) -> Vec<LoadBandStats> {
    grouped(runs, |run| {
        run.measures
            .load_band
            .map(|band| (load_band_order(band), band))
    })
    .into_iter()
    .map(|(band, intervals)| LoadBandStats {
        band: band.map(|(_, band)| band),
        intervals,
    })
    .collect()
}

/// The `backend_call_failed` of one load band.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BandCount {
    pub band: &'static str,
    pub count: i64,
}

/// Count a failure recorded under `load` in its band of `bands`, kept
/// lightest first.
pub(super) fn count_band(bands: &mut Vec<BandCount>, load: f64) {
    let band = load_band(load);
    match bands.iter_mut().find(|count| count.band == band) {
        Some(count) => count.count += 1,
        None => {
            bands.push(BandCount { band, count: 1 });
            bands.sort_by_key(|count| load_band_order(count.band));
        }
    }
}

/// How long one verification command took in `integrate`, over the
/// commands recorded with their duration in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandStats {
    pub command: String,
    pub count: usize,
    /// Of those, the ones that exited non-zero.
    pub failed: usize,
    pub total_secs: f64,
    pub median_secs: Option<f64>,
}

/// The `verification_command` events of `integrate` with `after < id <=
/// upto` whose task `counts` accepts, per command, by command.
pub(super) fn verification_commands(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> Vec<CommandStats> {
    let mut by_command: BTreeMap<&str, (Vec<f64>, usize)> = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.kind == "verification_command"
            && event.id > after
            && event.id <= upto
            && counts(event.task_id)
            && event.payload["phase"] == "integration"
    }) {
        let (Some(command), Some(secs)) = (
            event.payload["command"].as_str(),
            event.payload["duration_secs"].as_f64(),
        ) else {
            continue;
        };
        let entry = by_command.entry(command).or_default();
        entry.0.push(secs);
        if event.payload["exit_code"].as_i64() != Some(0) {
            entry.1 += 1;
        }
    }
    by_command
        .into_iter()
        .map(|(command, (mut secs, failed))| CommandStats {
            command: command.to_owned(),
            count: secs.len(),
            failed,
            total_secs: (secs.iter().sum::<f64>() * 1000.0).round() / 1000.0,
            median_secs: median_f64(&mut secs),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: "1970-01-01T00:00:00.000Z".to_owned(),
        }
    }

    #[test]
    fn a_run_without_measures_has_none() {
        let mut track = MeasureTrack::default();
        track.observe(&event(1, "run_claimed", json!({"from": "ready"})));
        track.observe(&event(2, "receipt_observed", json!({"validated": false})));
        // A later receipt's load is not the work's.
        track.observe(&event(2, "receipt_observed", json!({"load_avg_mean": 1.0})));
        track.observe(&event(2, "validation_finished", json!({})));
        track.observe(&event(
            2,
            "validation_finished",
            json!({"load_avg_mean": 1.0}),
        ));
        track.observe(&event(
            3,
            "verification_command",
            json!({"phase": "integration", "exit_code": 0}),
        ));
        assert_eq!(track.finish(), RunMeasures::default());
    }

    #[test]
    fn the_verify_load_weighs_each_command_by_its_duration() {
        let mut track = MeasureTrack::default();
        track.observe(&event(
            1,
            "verification_command",
            json!({"phase": "integration", "duration_secs": 30.0, "load_avg_mean": 10.0, "load_avg_max": 12.0}),
        ));
        track.observe(&event(
            2,
            "verification_command",
            json!({"phase": "integration", "load_avg_mean": 40.0, "load_avg_max": 50.0}),
        ));
        track.observe(&event(
            3,
            "verification_command",
            json!({"phase": "recheck", "duration_secs": 1.0, "load_avg_mean": 99.0}),
        ));
        track.observe(&event(4, "run_claimed", json!({"load_avg": 70.0})));
        let measures = track.finish();
        assert_eq!(
            measures.load.verify,
            Some(IntervalLoad {
                mean: Some(10.97),
                max: Some(50.0),
                band: Some("8-16"),
            })
        );
        // No work load: the band is the claim's.
        assert_eq!(measures.load_band, Some("64+"));
    }

    #[test]
    fn failures_count_in_their_band_lightest_first() {
        let mut bands = Vec::new();
        for load in [70.0, 2.0, 5.0, 3.0] {
            count_band(&mut bands, load);
        }
        assert_eq!(
            bands,
            vec![
                BandCount {
                    band: "0-4",
                    count: 2
                },
                BandCount {
                    band: "4-8",
                    count: 1
                },
                BandCount {
                    band: "64+",
                    count: 1
                },
            ]
        );
    }
}
