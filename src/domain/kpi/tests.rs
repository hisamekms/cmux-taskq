use std::collections::HashMap;

use serde_json::{Value, json};

use super::config::{JudgedPeriod, KpiSettings, judge};
use super::*;
use crate::domain::{EventId, RunId};

/// 2026-09-21T00:00:00+09:00, a Monday, in unix seconds.
const MONDAY: i64 = 1_789_916_400;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const JST: i64 = 9 * HOUR;

/// The events of a queue, built in time order.
#[derive(Default)]
struct Queue {
    events: Vec<RunEvent>,
    kinds: HashMap<TaskId, Option<TaskKind>>,
    goals: HashMap<TaskId, Option<GoalId>>,
}

/// How one run went.
#[derive(Clone)]
struct Run {
    task: i64,
    kind: Option<TaskKind>,
    /// Unix seconds of the claim.
    claimed: i64,
    work: i64,
    parallel: i64,
    slots: i64,
    load: f64,
    build: &'static str,
    revise: bool,
    failed: bool,
}

impl Run {
    fn new(task: i64, kind: Option<TaskKind>, claimed: i64, work: i64) -> Self {
        Self {
            task,
            kind,
            claimed,
            work,
            parallel: 3,
            slots: 1,
            load: 2.0,
            build: "b1",
            revise: false,
            failed: false,
        }
    }
}

impl Queue {
    fn push(
        &mut self,
        task: Option<i64>,
        run: Option<&str>,
        kind: &str,
        payload: Value,
        secs: i64,
    ) {
        self.events.push(RunEvent {
            id: EventId::new(self.events.len() as i64 + 1),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: marks::utc_text(secs * 1000),
        });
    }

    fn queue_event(&mut self, kind: &str, payload: Value, secs: i64) {
        self.push(None, None, kind, payload, secs);
    }

    /// A run that lands (or fails) `work` seconds after its claim, the task
    /// ready an hour before; returns the landing time.
    fn run(&mut self, run: &Run) -> i64 {
        let id = format!("{:08x}-0000-4000-8000-{:012x}", run.task, run.claimed);
        let (task, id) = (Some(run.task), Some(id.as_str()));
        self.kinds.insert(TaskId::new(run.task), run.kind);
        self.goals.insert(TaskId::new(run.task), None);
        self.push(
            task,
            None,
            "task_status_changed",
            json!({"from": "submitted", "to": "ready"}),
            run.claimed - HOUR,
        );
        self.push(
            task,
            id,
            "run_claimed",
            json!({"parallel": run.parallel, "slots": run.slots, "load_avg": run.load, "dagq_version": run.build}),
            run.claimed,
        );
        self.push(task, id, "agent_started", json!({}), run.claimed + 10);
        self.push(
            task,
            id,
            "first_commit_observed",
            json!({}),
            run.claimed + 60,
        );
        let receipt = run.claimed + run.work;
        if run.failed {
            self.push(task, id, "run_failed", json!({"status": "failed"}), receipt);
            return receipt;
        }
        self.push(task, id, "receipt_observed", json!({}), receipt);
        self.push(
            task,
            id,
            "validation_finished",
            json!({"status": "awaiting_integration"}),
            receipt + 20,
        );
        if run.revise {
            self.push(task, id, "revise_requested", json!({}), receipt + 30);
        }
        self.push(task, id, "integration_started", json!({}), receipt + 40);
        self.push(task, id, "integration_rebased", json!({}), receipt + 50);
        self.push(
            task,
            id,
            "run_integrated",
            json!({"status": "integrated"}),
            receipt + 100,
        );
        receipt + 100
    }

    /// The events in time order, renumbered, as a queue records them; a
    /// retraction keeps naming its mark.
    fn sort(&mut self) {
        self.events
            .sort_by_key(|event| timestamp_millis(&event.created_at));
        let renumbered: HashMap<i64, i64> = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.as_i64(), index as i64 + 1))
            .collect();
        for event in &mut self.events {
            event.id = EventId::new(renumbered[&event.id.as_i64()]);
            if let Some(mark) = event.payload.get("mark").and_then(Value::as_i64) {
                event.payload["mark"] = json!(renumbered[&mark]);
            }
        }
    }

    /// The id of the mark labeled `label`, once sorted.
    fn mark_id(&mut self, label: &str) -> EventId {
        self.sort();
        self.events
            .iter()
            .find(|event| event.payload["label"] == label)
            .unwrap()
            .id
    }

    fn kpi(&mut self, now: i64, config: &KpiConfig, query: &KpiQuery) -> Kpi {
        self.sort();
        kpi(
            &KpiInput {
                events: &self.events,
                goals: &self.goals,
                kinds: &self.kinds,
                now,
                utc_offset_secs: JST,
                cores: Some(4),
                config,
            },
            query,
        )
        .unwrap()
    }
}

fn measure<'a>(period: &'a PeriodKpis, name: &str, stratum: &str) -> &'a Measure {
    &period.window.kpis[name][stratum]
}

/// Days start at local midnight and weeks on Monday; the labels are the
/// local date and the ISO week, across a year's end too.
#[test]
fn periods_start_at_local_midnight_and_monday() {
    let offset = JST * 1000;
    let noon = (MONDAY + 2 * DAY + 12 * HOUR) * 1000;
    let day = Period::Day.start(noon, offset);
    assert_eq!(day, (MONDAY + 2 * DAY) * 1000);
    assert_eq!(Period::Day.label(day, offset), "2026-09-23");
    // 08:59 in Tokyo is still the day before in UTC, but the local day's.
    let early = (MONDAY + 2 * DAY + 30 * 60) * 1000;
    assert_eq!(Period::Day.start(early, offset), day);
    let week = Period::Week.start(noon, offset);
    assert_eq!(week, MONDAY * 1000);
    assert_eq!(Period::Week.label(week, offset), "2026-W39");
    // 2027-01-01 is a Friday: its week is 2026's 53rd.
    let new_year = marks_ms("2027-01-01T12:00:00Z");
    assert_eq!(
        Period::Week.label(Period::Week.start(new_year, 0), 0),
        "2026-W53"
    );
    assert_eq!("week".parse::<Period>(), Ok(Period::Week));
    assert!("month".parse::<Period>().is_err());
    assert_eq!("load".parse::<Axis>(), Ok(Axis::Load));
    assert!("host".parse::<Axis>().unwrap_err().contains("toolchain"));
}

fn marks_ms(text: &str) -> i64 {
    timestamp_millis(text).unwrap()
}

/// Spreads follow `stats`' rules, a rate over nothing is null, and a count
/// is judged whatever its size.
#[test]
fn measures_keep_null_apart_from_zero() {
    let secs = Measure::secs([40, 10, 30, 20]);
    assert_eq!(
        (secs.n, secs.median, secs.p90, secs.min, secs.max),
        (4, Some(25.0), Some(40.0), Some(10.0), Some(40.0))
    );
    assert_eq!(Measure::secs([]).median, None);
    let rate = Measure::ratio(1.0, 3);
    assert_eq!(rate.value, Some(0.333));
    assert_eq!(Measure::ratio(0.0, 0).value, None);
    assert_eq!(
        serde_json::to_value(Measure::ratio(0.0, 0)).unwrap(),
        json!({"n": 0, "value": null})
    );
    let spread = Measure::spread([1.5, 0.5, 2.25]);
    assert_eq!((spread.median, spread.p90), (Some(1.5), Some(2.25)));
    assert!(Measure::count(1).enough(5));
    assert!(!secs.enough(5));
    assert_eq!(secs.primary(), Some(25.0));
    assert_eq!(Measure::count(2).primary(), Some(2.0));
    assert_eq!(direction("landings"), Some(Direction::Higher));
    assert_eq!(direction("phase.work"), Some(Direction::Lower));
    assert_eq!(direction("session_open.inbox"), None);
    assert_eq!(direction("session_open.worker"), Some(Direction::Lower));
    assert_eq!(direction("auto_repairs"), None);
}

/// Each run falls in the day it finished; its KPIs are split by the task's
/// kind (a task without one is `unknown`) and the claim's attributes, and
/// each day sits next to the previous one.
#[test]
fn splits_the_runs_by_kind_and_attributes_per_day() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY;
    let mut runs = Vec::new();
    for (index, kind) in [
        Some(TaskKind::Runtime),
        Some(TaskKind::Runtime),
        Some(TaskKind::Docs),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let index = index as i64;
        let mut run = Run::new(
            10 + index,
            kind,
            tuesday + HOUR * (index + 1),
            600 * (index + 1),
        );
        run.parallel = 2 + index % 2;
        run.load = 1.0 + 4.0 * index as f64;
        run.revise = index == 1;
        runs.push(run);
    }
    for run in &runs {
        queue.run(run);
    }
    // Monday: one runtime run, and one that failed.
    queue.run(&Run::new(1, Some(TaskKind::Runtime), MONDAY + HOUR, 300));
    let mut failed = Run::new(2, Some(TaskKind::Runtime), MONDAY + 2 * HOUR, 300);
    failed.failed = true;
    queue.run(&failed);
    let query = KpiQuery {
        last: 2,
        at: Some(Cursor::Time(tuesday * 1000)),
        by: vec![Axis::Parallel, Axis::Load],
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let result = queue.kpi(tuesday + DAY + HOUR, &config, &query);
    assert_eq!(result.period, "day");
    let [monday, tuesday] = [&result.periods[0], &result.periods[1]];
    assert_eq!(
        (monday.label.as_str(), tuesday.label.as_str()),
        ("2026-09-21", "2026-09-22")
    );
    assert!(!tuesday.partial);
    assert_eq!(tuesday.window.runs, 4);
    assert_eq!(measure(tuesday, "landings", ALL).value, Some(4.0));
    assert_eq!(
        measure(tuesday, "landings", "kind=runtime").value,
        Some(2.0)
    );
    assert_eq!(measure(tuesday, "landings", "kind=docs").value, Some(1.0));
    assert_eq!(
        measure(tuesday, "landings", "kind=unknown").value,
        Some(1.0)
    );
    let work = measure(tuesday, "phase.work", "kind=runtime");
    assert_eq!(
        (work.n, work.median, work.max),
        (2, Some(900.0), Some(1200.0))
    );
    assert_eq!(measure(tuesday, "phase.startup", ALL).median, Some(50.0));
    // Ready an hour before the claim, landed 120 s after the receipt.
    assert_eq!(
        measure(tuesday, "lead_time", "kind=docs").median,
        Some(3600.0 + 1800.0 + 100.0)
    );
    assert_eq!(
        measure(tuesday, "revise_rate", "kind=runtime").value,
        Some(0.5)
    );
    assert_eq!(measure(tuesday, "first_pass_rate", ALL).value, Some(0.75));
    assert_eq!(
        measure(tuesday, "verification_failed_rate", ALL).value,
        Some(0.0)
    );
    assert_eq!(measure(tuesday, "verification_failed_rate", ALL).n, 4);
    // parallel alternates 2, 3; the load per core (4 cores) 0.25, 1.25, 2.25, 3.25.
    assert_eq!(measure(tuesday, "landings", "parallel=2").value, Some(2.0));
    assert_eq!(measure(tuesday, "landings", "load=low").value, Some(1.0));
    assert_eq!(measure(tuesday, "landings", "load=mid").value, Some(1.0));
    assert_eq!(measure(tuesday, "landings", "load=high").value, Some(2.0));
    assert_eq!(measure(tuesday, "max_load_avg", ALL).value, Some(13.0));
    assert_eq!(measure(monday, "failed_rate", ALL).value, Some(0.5));
    assert_eq!(measure(monday, "landings", ALL).value, Some(1.0));
    // Tuesday against Monday: a count is judged, a small spread is not.
    let landings = &tuesday.comparison["landings"][ALL];
    assert_eq!(
        (landings.previous, landings.delta, landings.ratio),
        (Some(1.0), Some(3.0), Some(4.0))
    );
    assert_eq!(
        (landings.judged, landings.verdict),
        (true, Some("improved"))
    );
    let work = &tuesday.comparison["phase.work"][ALL];
    assert_eq!((work.judged, work.reason), (false, Some("small_sample")));
    assert_eq!(
        tuesday.comparison["landings"]["kind=docs"].reason,
        Some("no_value")
    );
    // The 7 days before Tuesday: six empty days and Monday.
    assert_eq!(landings.baseline_7d, Some(0.0));
    // Listing only the docs strata leaves the others and `all`.
    let docs = queue.kpi(
        tuesday.end_ms() / 1000 + HOUR,
        &config,
        &KpiQuery {
            kinds: vec!["docs".into()],
            ..query.clone()
        },
    );
    let strata: Vec<&String> = docs.periods[1].window.kpis["landings"].keys().collect();
    assert!(strata.contains(&&"kind=docs".to_owned()) && strata.contains(&&ALL.to_owned()));
    assert!(!strata.contains(&&"kind=runtime".to_owned()));
}

impl PeriodKpis {
    fn end_ms(&self) -> i64 {
        timestamp_millis(&self.end).unwrap()
    }
}

/// Today is listed as partial, and no target is judged on it.
#[test]
fn the_period_not_over_yet_is_partial() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY + HOUR, 300));
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            targets: vec![Target {
                kpi: "landings".into(),
                kind: None,
                stat: None,
                min: Some(1.0),
                max: None,
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let result = queue.kpi(MONDAY + 3 * HOUR, &config, &KpiQuery::default());
    let today = result.periods.last().unwrap();
    assert!(today.partial);
    assert_eq!(measure(today, "landings", ALL).value, Some(1.0));
    let target = &result.targets[0];
    assert_eq!(target.periods.last().unwrap().reason, Some("partial"));
    // Shown next to yesterday, but not judged.
    let landings = &today.comparison["landings"][ALL];
    assert_eq!(
        (landings.delta, landings.reason, landings.verdict),
        (Some(1.0), Some("partial"), None)
    );
    // The days before, with no landing, are judged and off target.
    assert_eq!(target.state, "breach");
    assert_eq!(target.periods[DEFAULT_LAST - 2].met, Some(false));
    assert_eq!(result.periods.len(), DEFAULT_LAST);
}

fn window_kpis(value: Option<f64>, n: usize) -> Kpis<Measure> {
    let mut measure = Measure::secs(std::iter::repeat_n(0, n));
    measure.median = value;
    let mut kpis = Kpis::new();
    kpis.entry("phase.work".into())
        .or_default()
        .insert("kind=runtime".into(), measure);
    kpis
}

/// A target breaks after `breach_periods` judged periods off target in a
/// row; a period with too few samples is skipped without breaking the
/// streak; one period off target is only `missed`, and one on target ends
/// the breach.
#[test]
fn a_breach_needs_consecutive_judged_periods_off_target() {
    let config = KpiConfig::merge(
        Some(&KpiSettings {
            min_samples: Some(3),
            targets: vec![Target {
                kpi: "phase.work".into(),
                kind: Some("runtime".into()),
                stat: Some(Stat::Median),
                min: None,
                max: Some(100.0),
            }],
            ..KpiSettings::default()
        }),
        None,
    );
    let days: Vec<(String, Kpis<Measure>)> = [
        (Some(50.0), 5),  // ok
        (Some(150.0), 5), // off
        (Some(150.0), 2), // too few: skipped
        (None, 0),        // nothing: skipped
        (Some(160.0), 4), // off
        (Some(170.0), 9), // off: the third in a row
    ]
    .into_iter()
    .enumerate()
    .map(|(day, (value, n))| (format!("d{day}"), window_kpis(value, n)))
    .collect();
    let periods = |upto: usize| -> Vec<JudgedPeriod<'_>> {
        days[..upto]
            .iter()
            .map(|(label, kpis)| JudgedPeriod {
                label,
                kpis,
                partial: false,
                listed: true,
            })
            .collect()
    };
    let breach = &judge(&config, &periods(6), 3)[0];
    assert_eq!(breach.state, "breach");
    assert_eq!(breach.streak, 3);
    assert_eq!(breach.breach_since.as_deref(), Some("d1"));
    assert_eq!(breach.stratum, "kind=runtime");
    assert_eq!(breach.source, "repository");
    let reasons: Vec<Option<&str>> = breach.periods.iter().map(|p| p.reason).collect();
    assert_eq!(
        reasons,
        [
            None,
            None,
            Some("small_sample"),
            Some("no_value"),
            None,
            None
        ]
    );
    assert_eq!(breach.periods[1].met, Some(false));
    assert_eq!(breach.periods[2].met, None);
    let missed = &judge(&config, &periods(2), 3)[0];
    assert_eq!((missed.state, missed.streak), ("missed", 1));
    assert_eq!(judge(&config, &periods(1), 3)[0].state, "ok");
    assert_eq!(judge(&config, &periods(0), 3)[0].state, "not_judged");
    // Two weeks off target are a breach at `breach_weeks` 2.
    assert_eq!(judge(&config, &periods(6)[4..], 2)[0].state, "breach");
    // A judged period on target after the streak ends the breach.
    let mut recovered = days.clone();
    recovered.push(("d6".into(), window_kpis(Some(90.0), 5)));
    let periods: Vec<JudgedPeriod<'_>> = recovered
        .iter()
        .map(|(label, kpis)| JudgedPeriod {
            label,
            kpis,
            partial: false,
            listed: false,
        })
        .collect();
    let report = &judge(&config, &periods, 3)[0];
    assert_eq!((report.state, report.streak), ("ok", 0));
    assert!(report.periods.is_empty());
}

/// The host's settings win over the repository's, but not its
/// `max_improvement_proposals`; a target of the same KPI and stratum is
/// the host's, and no target is built in.
#[test]
fn the_host_settings_win_over_the_repository() {
    let target = |kpi: &str, kind: Option<&str>, max: f64| Target {
        kpi: kpi.into(),
        kind: kind.map(Into::into),
        stat: None,
        min: None,
        max: Some(max),
    };
    let repository = KpiSettings {
        min_samples: Some(7),
        breach_periods: Some(4),
        max_improvement_proposals: Some(1),
        targets: vec![
            target("phase.work", Some("runtime"), 3600.0),
            target("revise_rate", None, 0.3),
        ],
        ..KpiSettings::default()
    };
    let host = KpiSettings {
        min_samples: Some(2),
        max_improvement_proposals: Some(9),
        targets: vec![target("phase.work", Some("runtime"), 1800.0)],
        ..KpiSettings::default()
    };
    let config = KpiConfig::merge(Some(&repository), Some(&host));
    assert_eq!(config.min_samples, 2);
    assert_eq!(config.breach_periods, 4);
    assert_eq!(config.breach_weeks, config::DEFAULT_BREACH_WEEKS);
    assert_eq!(config.max_improvement_proposals, 1);
    assert_eq!(config.sources["min_samples"], "host");
    assert_eq!(config.sources["breach_periods"], "repository");
    assert_eq!(config.sources["breach_weeks"], "default");
    assert_eq!(config.targets.len(), 2);
    assert_eq!(config.targets[0].target.max, Some(1800.0));
    assert_eq!(config.targets[0].source, "host");
    assert_eq!(config.targets[1].source, "repository");
    assert!(KpiConfig::default().targets.is_empty());
    let overlaid = repository.clone().overlay(host);
    assert_eq!(overlaid.min_samples, Some(2));
    assert_eq!(overlaid.targets.len(), 2);
}

fn start(queue: &mut Queue, parallel: i64, secs: i64) {
    queue.queue_event(
        marks::SUPERVISOR_STARTED,
        json!({"supervisor": "s", "parallel": parallel, "dagq_version": "b1"}),
        secs,
    );
}

/// A comparison at a mark: a window on each side, the other marks in and
/// between them listed, marks too close to split taken as one change,
/// and every side and stratum with its `n`, median, p90 and range.
#[test]
fn compares_across_a_mark_with_its_confounders_and_strata() {
    let mut queue = Queue::default();
    // Before: runtime runs of 1000 s at parallel 4.
    for index in 0..6 {
        let mut run = Run::new(
            100 + index,
            Some(TaskKind::Runtime),
            MONDAY + index * HOUR,
            1000,
        );
        run.parallel = 4;
        queue.run(&run);
    }
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "sccache", "by": "human"}),
        MONDAY + 8 * HOUR,
    );
    // No run between these: taken with the next as one change.
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "parallel 4→3", "by": "human"}),
        MONDAY + 9 * HOUR,
    );
    // After: faster runtime runs, the last on a new build at parallel 3,
    // and a docs run.
    for index in 0..6 {
        let mut run = Run::new(
            200 + index,
            Some(TaskKind::Runtime),
            MONDAY + (10 + index) * HOUR,
            600,
        );
        run.parallel = 4;
        if index == 5 {
            run.build = "b2";
            run.parallel = 3;
        }
        queue.run(&run);
    }
    let mut docs = Run::new(300, Some(TaskKind::Docs), MONDAY + 17 * HOUR, 100);
    docs.build = "b2";
    queue.run(&docs);
    // A later mark inside the window after, and a retracted one that is none.
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "host arm64", "by": "human"}),
        MONDAY + 30 * HOUR,
    );
    queue.queue_event(
        marks::MARK_RECORDED,
        json!({"label": "mistake", "by": "human"}),
        MONDAY + 31 * HOUR,
    );
    let mistake = queue.events.last().unwrap().id.as_i64();
    queue.queue_event(
        marks::MARK_RETRACTED,
        json!({"mark": mistake, "by": "human"}),
        MONDAY + 32 * HOUR,
    );
    let query = KpiQuery {
        last: 1,
        compare: Some(CompareSpec::At(Cursor::Event(
            queue.mark_id("parallel 4→3"),
        ))),
        window_days: 2,
        ..KpiQuery::default()
    };
    let config = KpiConfig::default();
    let result = queue.kpi(MONDAY + 5 * DAY, &config, &query);
    let compare = result.compare.unwrap();
    let split = compare.split.as_ref().unwrap();
    assert!(!split.separable);
    let labels: Vec<&str> = split.marks.iter().map(|mark| mark.label.as_str()).collect();
    assert_eq!(labels, ["sccache", "parallel 4→3"]);
    assert_eq!(
        timestamp_millis(&split.start),
        Some((MONDAY + 8 * HOUR) * 1000)
    );
    assert_eq!(
        timestamp_millis(&compare.before.end),
        Some((MONDAY + 8 * HOUR) * 1000)
    );
    assert_eq!(
        timestamp_millis(&compare.after.start),
        Some((MONDAY + 9 * HOUR) * 1000)
    );
    assert_eq!((compare.before.runs, compare.after.runs), (6, 7));
    assert!(!compare.after.partial);
    // The derived marks of the new build and parallel sit in the window
    // after, with the later person's mark; the retracted one is left out.
    // The three are one overlapping change: two runs finished between them.
    let confounders: Vec<(&str, &str)> = compare
        .confounders
        .iter()
        .map(|c| (c.position, c.mark.kind.as_str()))
        .collect();
    assert_eq!(
        confounders,
        [
            ("after", "derived:dagq_version"),
            ("after", "derived:parallel"),
            ("after", "mark_recorded"),
        ]
    );
    assert_eq!(compare.overlapping.len(), 2);
    assert_eq!(compare.overlapping[0].len(), 2);
    assert_eq!(compare.overlapping[1].len(), 3);
    let work = &compare.strata["phase.work"];
    let all = &work[ALL];
    assert_eq!((all.before.n, all.after.n), (6, 7));
    assert_eq!(
        (all.before.median, all.after.median),
        (Some(1000.0), Some(600.0))
    );
    assert_eq!(
        (all.after.min, all.after.max, all.after.p90),
        (Some(100.0), Some(600.0), Some(600.0))
    );
    assert_eq!(
        (all.change.judged, all.change.verdict),
        (true, Some("improved"))
    );
    for stratum in [
        "kind=runtime",
        "parallel=4",
        "parallel=3",
        "load=low",
        "build=b1",
        "build=b2",
    ] {
        assert!(work.contains_key(stratum), "{stratum}");
    }
    assert_eq!(
        (work["parallel=4"].before.n, work["parallel=4"].after.n),
        (6, 5)
    );
    assert!(work["parallel=4"].change.judged);
    // The new build's runtime run and the docs run.
    assert_eq!(
        (work["parallel=3"].before.n, work["parallel=3"].after.n),
        (0, 2)
    );
    assert_eq!(work["parallel=3"].change.reason, Some("no_value"));
    assert_eq!(work["build=b2"].after.n, 2);
    let docs = &work["kind=docs"];
    assert_eq!((docs.before.n, docs.after.n), (0, 1));
    // A derived mark is named by its claim: the new build's change is one
    // with the later person's mark, and none of them is its own confounder.
    let claim = compare.confounders[0].mark.detail["claim_event"]
        .as_i64()
        .unwrap();
    let derived = queue.kpi(
        MONDAY + 5 * DAY,
        &config,
        &KpiQuery {
            compare: Some(CompareSpec::At(Cursor::Event(EventId::new(claim)))),
            ..query.clone()
        },
    );
    let derived = derived.compare.unwrap();
    let split = derived.split.unwrap();
    assert_eq!((split.marks.len(), split.separable), (3, false));
    assert!(derived.confounders.iter().all(|c| c.position == "before"));
    // The summary is the runtime runs' times only.
    let summary = &compare.summary["runtime"];
    assert_eq!(summary["phase.work"].after.median, Some(600.0));
    assert!(!summary.contains_key("landings"));
}

/// Marks join one change while fewer than `min_samples` runs finish
/// between them; three or more in a row are one.
#[test]
fn marks_too_close_are_one_overlapping_change() {
    let mark = |secs: i64, label: &str| Mark {
        id: None,
        kind: "mark_recorded".into(),
        at: marks::utc_text(secs * 1000),
        recorded_at: marks::utc_text(secs * 1000),
        label: label.into(),
        retracted_by: None,
        detail: Value::Null,
    };
    let marks = [
        mark(100, "a"),
        mark(200, "b"),
        mark(300, "c"),
        mark(1000, "d"),
    ];
    let finishes: Vec<i64> = [150, 250, 400, 500, 600].map(|s| s * 1000).to_vec();
    let groups = compare::overlapping_groups(&marks, &finishes, 2);
    let labels: Vec<Vec<&str>> = groups
        .iter()
        .map(|group| group.iter().map(|m| m.label.as_str()).collect())
        .collect();
    assert_eq!(labels, [vec!["a", "b", "c"], vec!["d"]]);
    assert_eq!(compare::overlapping_groups(&marks, &finishes, 1).len(), 4);
}

/// Two explicit windows compare with no split, and a malformed
/// `--compare` is refused.
#[test]
fn compares_two_explicit_windows() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY + DAY, 200));
    let spec: CompareSpec = format!(
        "@{}..@{},@{}..@{}",
        MONDAY - HOUR,
        MONDAY + HOUR,
        MONDAY + DAY - HOUR,
        MONDAY + DAY + HOUR
    )
    .parse()
    .unwrap();
    let query = KpiQuery {
        compare: Some(spec),
        last: 1,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 2 * DAY, &KpiConfig::default(), &query);
    let compare = result.compare.unwrap();
    assert!(compare.split.is_none());
    assert_eq!(compare.strata["phase.work"][ALL].after.median, Some(200.0));
    assert!("1..2".parse::<CompareSpec>().is_err());
    assert!("x".parse::<CompareSpec>().is_err());
    assert_eq!(
        "12".parse::<CompareSpec>(),
        Ok(CompareSpec::At(Cursor::Event(EventId::new(12))))
    );
    let backwards: CompareSpec = format!("@{}..@{},@1..@2", MONDAY + HOUR, MONDAY)
        .parse()
        .unwrap();
    let error = kpi(
        &KpiInput {
            events: &queue.events,
            goals: &queue.goals,
            kinds: &queue.kinds,
            now: MONDAY + 2 * DAY,
            utc_offset_secs: 0,
            cores: None,
            config: &KpiConfig::default(),
        },
        &KpiQuery {
            compare: Some(backwards),
            ..KpiQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("start before it ends"), "{error}");
}

/// `--since` / `--until` give one window next to the one of the same
/// length before it.
#[test]
fn one_window_of_any_length() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY + 3 * HOUR, 100));
    let query = KpiQuery {
        since: Some(Cursor::Time((MONDAY + HOUR) * 1000)),
        until: Some(Cursor::Time((MONDAY + 5 * HOUR) * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    assert_eq!(result.period, "window");
    assert_eq!(result.periods.len(), 1);
    let window = &result.periods[0];
    assert!(!window.partial);
    assert_eq!(measure(window, "landings", ALL).value, Some(1.0));
    assert_eq!(window.comparison["landings"][ALL].previous, Some(1.0));
    let error = kpi(
        &KpiInput {
            events: &queue.events,
            goals: &queue.goals,
            kinds: &queue.kinds,
            now: MONDAY + DAY,
            utc_offset_secs: 0,
            cores: None,
            config: &KpiConfig::default(),
        },
        &KpiQuery {
            since: query.until,
            until: query.since,
            ..KpiQuery::default()
        },
    )
    .unwrap_err();
    assert!(error.contains("before --until"), "{error}");
}

/// The KPIs no run carries: slot usage while a supervisor lived, the
/// candidates' samples, asks per landing and a person's wait, attentions,
/// findings, and the ones not recorded yet.
#[test]
fn derives_the_queue_kpis_of_a_window() {
    let mut queue = Queue::default();
    start(&mut queue, 2, MONDAY);
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"candidates": 2, "free_slots": 1, "ready": 2}),
        MONDAY,
    );
    // Two hours of one run in four hours of two slots.
    queue.run(&Run::new(
        1,
        Some(TaskKind::Runtime),
        MONDAY + HOUR,
        2 * HOUR - 100,
    ));
    queue.queue_event(
        window::CANDIDATES_SAMPLED,
        json!({"candidates": 0, "free_slots": 1, "ready": 1}),
        MONDAY + 2 * HOUR,
    );
    queue.queue_event(
        marks::SUPERVISOR_STOPPED,
        json!({"supervisor": "s"}),
        MONDAY + 4 * HOUR,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 7, "kind": "worker_question", "reason_category": "scope"}),
        MONDAY + 90 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 7, "answered_by": "human"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_opened",
        json!({"ask_id": 8, "kind": "stuck_exit"}),
        MONDAY + 100 * 60,
    );
    queue.push(
        Some(1),
        None,
        "ask_answered",
        json!({"ask_id": 8, "runtime_closed": true}),
        MONDAY + 101 * 60,
    );
    queue.queue_event("finding_recorded", json!({"finding_id": 1}), MONDAY + HOUR);
    queue.queue_event("finding_recorded", json!({"finding_id": 2}), MONDAY + HOUR);
    queue.queue_event(
        "finding_status_changed",
        json!({"finding_id": 1, "from": "open", "to": "resolved"}),
        MONDAY + 3 * HOUR,
    );
    let query = KpiQuery {
        at: Some(Cursor::Time(MONDAY * 1000)),
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + DAY, &KpiConfig::default(), &query);
    let day = result.periods.last().unwrap();
    assert!(!day.partial);
    assert_eq!(measure(day, "slot_usage", ALL).value, Some(0.25));
    assert_eq!(measure(day, "slot_usage", ALL).n, 1);
    // 2 candidates for two hours, then none until now (22 hours).
    let candidates = measure(day, "candidates", ALL);
    assert_eq!(candidates.value, Some(round3(4.0 / 24.0)));
    assert_eq!(candidates.max, Some(2.0));
    assert_eq!(
        day.window.details["candidates"]["starved_secs"],
        json!(22 * HOUR)
    );
    assert_eq!(measure(day, "asks_per_landing", ALL).value, Some(2.0));
    assert_eq!(
        measure(day, "asks_per_landing", "kind=runtime").value,
        Some(2.0)
    );
    // The runtime's own close is no person's wait.
    let wait = measure(day, "ask_wait", ALL);
    assert_eq!((wait.n, wait.median), (1, Some(600.0)));
    assert_eq!(measure(day, "findings_open", ALL).value, Some(1.0));
    assert_eq!(
        measure(day, "finding_resolve_time", ALL).median,
        Some(7200.0)
    );
    assert_eq!(day.window.details["findings"]["recorded"], json!(2));
    assert!(measure(day, "attentions_per_landing", ALL).value.is_some());
    assert_eq!(
        day.window.unavailable["improvement_proposals"],
        "not_recorded"
    );
    assert!(!day.window.unavailable.contains_key("candidates"));
    // A day before the queue: no supervisor, no sample.
    let before = &result.periods[0];
    assert_eq!(measure(before, "slot_usage", ALL).value, None);
    assert_eq!(before.window.unavailable["candidates"], "no_samples");
    assert_eq!(measure(before, "landings", ALL).value, Some(0.0));
}

/// With `--goal`, only that goal's runs count.
#[test]
fn a_goal_keeps_only_its_runs() {
    let mut queue = Queue::default();
    queue.run(&Run::new(1, None, MONDAY, 100));
    queue.run(&Run::new(2, None, MONDAY, 100));
    queue.goals.insert(TaskId::new(1), Some(GoalId::new(5)));
    let query = KpiQuery {
        goal_id: Some(GoalId::new(5)),
        last: 1,
        ..KpiQuery::default()
    };
    let result = queue.kpi(MONDAY + 12 * HOUR, &KpiConfig::default(), &query);
    assert_eq!(
        measure(&result.periods[0], "landings", ALL).value,
        Some(1.0)
    );
    assert_eq!(result.config.min_samples, config::DEFAULT_MIN_SAMPLES);
}

/// The quality of the plans per day (ADR-0079 decision 7): the revises of
/// plan review split by the model and effort of its session, and the
/// proposal's tasks' rework and duplicates by the review that judged it and
/// the proposal's features; the follow-ups adopted as `draft_flow` counts
/// them.
#[test]
fn plan_quality_is_split_by_the_judging_session_and_the_proposal() {
    let mut queue = Queue::default();
    let tuesday = MONDAY + DAY + 10 * HOUR;
    let features = |revise_count: i64| {
        json!({"origin": "person", "follow_up_depth": 0, "related_score": 4.0,
               "related": "mid", "revise_count": revise_count})
    };
    let review = |queue: &mut Queue, review: i64, effort: &str, decision: &str, at: i64| {
        queue.push(
            Some(10),
            None,
            "plan_review_started",
            json!({"proposal_id": 1, "plan_review_id": review,
                   "features": features(review - 1)}),
            at,
        );
        queue.push(
            Some(10),
            None,
            "session_opened",
            json!({"kind": "plan_review", "plan_review_id": review, "marker": review}),
            at,
        );
        queue.push(
            Some(10),
            None,
            "plan_review_finished",
            json!({"proposal_id": 1, "plan_review_id": review, "decision": decision}),
            at + 60,
        );
        queue.push(
            Some(10),
            None,
            "session_closed",
            json!({"kind": "plan_review", "opened_marker": review,
                   "model": "claude-opus-5-5", "effort": effort}),
            at + 60,
        );
    };
    for task in [10, 11] {
        queue.push(
            Some(task),
            None,
            "task_submitted",
            json!({"proposal_id": 1}),
            tuesday - 60,
        );
    }
    review(&mut queue, 1, "medium", "revise", tuesday);
    review(&mut queue, 2, "high", "pass", tuesday + HOUR);
    // Task 10 is a follow-up draft adopted into the proposal; task 11's run
    // is sent back to revise; 10 lands.
    queue.push(
        Some(9),
        None,
        "follow_up_registered",
        json!({"task_id": 10}),
        tuesday - 2 * HOUR,
    );
    queue.push(
        Some(10),
        None,
        "task_created",
        json!({}),
        tuesday - 2 * HOUR,
    );
    queue.push(
        Some(10),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "submitted"}),
        tuesday - HOUR,
    );
    let mut revised = Run::new(11, Some(TaskKind::Runtime), tuesday + 3 * HOUR, 600);
    revised.revise = true;
    queue.run(&revised);
    queue.run(&Run::new(
        10,
        Some(TaskKind::Runtime),
        tuesday + 3 * HOUR,
        300,
    ));
    queue.sort();
    // Link each session_closed to its session_opened by the ids sorting
    // gave them.
    let opened: HashMap<i64, i64> = queue
        .events
        .iter()
        .filter(|e| e.kind == "session_opened")
        .map(|e| (e.payload["marker"].as_i64().unwrap(), e.id.as_i64()))
        .collect();
    for event in &mut queue.events {
        if let Some(marker) = event.payload.get("opened_marker").and_then(Value::as_i64) {
            event.payload["opened_event_id"] = json!(opened[&marker]);
        }
    }
    let kpi = queue.kpi(
        MONDAY + 2 * DAY + HOUR,
        &KpiConfig::default(),
        &KpiQuery {
            last: 2,
            ..KpiQuery::default()
        },
    );
    let day = &kpi.periods[0];
    assert_eq!(day.label, "2026-09-22");
    let value = |name: &str, stratum: &str| {
        let measure = measure(day, name, stratum);
        (measure.n, measure.value)
    };
    assert_eq!(value("plan.revise_rate", "all"), (2, Some(0.5)));
    assert_eq!(value("plan.revise_rate", "effort=medium"), (1, Some(1.0)));
    assert_eq!(value("plan.revise_rate", "effort=high"), (1, Some(0.0)));
    assert_eq!(value("plan.revise_rate", "revise_count=0"), (1, Some(1.0)));
    // The proposal was judged by the high review of its second submission.
    assert_eq!(
        value("plan.task_rework_rate", "effort=high"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "model=claude-opus-5-5"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "related=mid"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "revise_count=1"),
        (2, Some(0.5))
    );
    assert_eq!(
        value("plan.task_rework_rate", "origin=person"),
        (2, Some(0.5))
    );
    assert_eq!(value("plan.task_rework_rate", "effort=medium"), (0, None));
    assert_eq!(
        value("plan.duplicate_cancels_after_ready", "effort=high"),
        (1, Some(0.0))
    );
    assert_eq!(
        value("plan.follow_up_canceled_after_adoption", "effort=high"),
        (1, Some(0.0))
    );
    assert_eq!(value("plan.follow_up_adoption_rate", "all"), (1, Some(1.0)));
    // The next day has no review: its rate is null, next to the previous.
    let next = &kpi.periods[1];
    assert_eq!(measure(next, "plan.revise_rate", "all").value, None);
    assert_eq!(
        next.comparison["plan.revise_rate"]["all"].previous,
        Some(0.5)
    );
    assert_eq!(direction("plan.task_rework_rate"), Some(Direction::Lower));
    assert_eq!(direction("plan.follow_up_adoption_rate"), None);
}
