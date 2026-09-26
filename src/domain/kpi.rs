//! `kpi` (ADR-0051 decisions 1–9 and 14–19): the KPIs of the flow, the
//! rework, the people's load, the infrastructure, the improvements and the
//! Claude sessions, derived by fixed rules from `run_events` for each day
//! or ISO week, split by the kind of task and by the attributes a run was
//! claimed with, next to the previous period, compared across a change
//! mark, and judged against the targets. A pure function of the events,
//! the task → goal and task → kind maps, the time zone, the host's cores
//! and the `[kpi]` settings, like [`super::stats`]: each window's runs,
//! intervals, `land_phases`, asks and sessions are what
//! [`super::stats::stats`] derives for it, and no rule is written twice.
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde::ser::SerializeMap;

use super::{
    GoalId, RunEvent, TaskId, TaskKind,
    marks::{self, Mark},
    stats::{Cursor, landing::p90, median, median_f64, timestamp_millis},
};

pub mod compare;
pub mod config;
mod window;

pub use compare::{Comparison, Confounder, Side, Split, WindowSpan};
pub use config::{KpiConfig, KpiSettings, Stat, Target, TargetReport};
pub use window::WindowKpis;

use window::Context;

pub const DAY_MS: i64 = 86_400_000;
/// The periods listed without `--last`.
pub const DEFAULT_LAST: usize = 7;
/// Each side of a comparison across a mark without `--window`, in days.
pub const DEFAULT_WINDOW_DAYS: i64 = 7;
/// The stratum of every run.
pub const ALL: &str = "all";
/// The value of an axis a run did not record, and the kind of a task
/// registered without one (decision 5).
pub const UNKNOWN: &str = "unknown";
/// The days before a day's period its `baseline_7d` is the median of.
const BASELINE_DAYS: usize = 7;

/// The length of a period (decision 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// The host's local day, from midnight.
    Day,
    /// The ISO week, from Monday midnight.
    Week,
}

impl Period {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
        }
    }

    /// The start of the period that holds `ms`, `offset_ms` east of UTC.
    pub fn start(self, ms: i64, offset_ms: i64) -> i64 {
        let local_days = (ms + offset_ms).div_euclid(DAY_MS);
        let days = match self {
            Self::Day => local_days,
            // 1970-01-01 was a Thursday: Monday is day 0 of the week.
            Self::Week => local_days - (local_days + 3).rem_euclid(7),
        };
        days * DAY_MS - offset_ms
    }

    fn length(self) -> i64 {
        match self {
            Self::Day => DAY_MS,
            Self::Week => 7 * DAY_MS,
        }
    }

    /// `YYYY-MM-DD` for a day, `YYYY-Www` for an ISO week, of the period
    /// that starts at `start`.
    pub fn label(self, start: i64, offset_ms: i64) -> String {
        let local_days = (start + offset_ms).div_euclid(DAY_MS);
        match self {
            Self::Day => date(local_days),
            Self::Week => {
                // The ISO year is the year of the week's Thursday.
                let thursday = local_days + 3;
                let year = &date(thursday)[..4];
                let january_first = timestamp_millis(&format!("{year}-01-01T00:00:00Z"))
                    .map_or(thursday, |ms| ms.div_euclid(DAY_MS));
                format!("{year}-W{:02}", (thursday - january_first) / 7 + 1)
            }
        }
    }

    /// The periods kept before the ones listed: the previous period, a
    /// day's 7-day baseline, and what the targets' streaks look back on.
    fn lookback(self) -> usize {
        match self {
            Self::Day => BASELINE_DAYS,
            Self::Week => 2,
        }
    }
}

impl std::str::FromStr for Period {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            _ => Err(format!("{text:?} is not a period; use day or week")),
        }
    }
}

fn date(days: i64) -> String {
    marks::utc_text(days * DAY_MS)[..10].to_owned()
}

/// What a run's KPIs can be split by besides `all` (decision 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Axis {
    /// The task's kind; `unknown` without one.
    Kind,
    /// The build identifier the run was claimed by.
    Build,
    /// The supervisor's `parallel` at the claim.
    Parallel,
    /// The slots in use at the claim over `parallel`: `low` (< 0.5), `mid`
    /// (< 1) or `full`.
    Slot,
    /// The load average at the claim over the host's cores: `low` (< 1),
    /// `mid` (< 2), `high` (< 4) or `extreme`.
    Load,
    /// `rustc`'s release and host.
    Toolchain,
    /// Claude Code's version.
    Claude,
}

impl Axis {
    pub const ALL: [Self; 7] = [
        Self::Kind,
        Self::Build,
        Self::Parallel,
        Self::Slot,
        Self::Load,
        Self::Toolchain,
        Self::Claude,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kind => "kind",
            Self::Build => "build",
            Self::Parallel => "parallel",
            Self::Slot => "slot",
            Self::Load => "load",
            Self::Toolchain => "toolchain",
            Self::Claude => "claude",
        }
    }
}

impl std::str::FromStr for Axis {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|axis| axis.as_str() == text)
            .ok_or_else(|| {
                format!(
                    "{text:?} is not an axis; use one of {}",
                    Self::ALL.map(Self::as_str).join(", ")
                )
            })
    }
}

/// The axes a comparison across a mark is split by (decision 15).
pub const COMPARE_AXES: [Axis; 4] = [Axis::Kind, Axis::Parallel, Axis::Load, Axis::Build];

/// What `--compare` names (decision 14): a mark (its event id) or any
/// time to split at, or two windows `A..B,C..D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareSpec {
    At(Cursor),
    Windows([(Cursor, Cursor); 2]),
}

impl std::str::FromStr for CompareSpec {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if !text.contains("..") {
            return text.parse().map(Self::At);
        }
        let window = |part: &str| -> Result<(Cursor, Cursor), String> {
            let (from, to) = part
                .split_once("..")
                .ok_or_else(|| format!("{part:?} is not a window FROM..TO"))?;
            Ok((from.parse()?, to.parse()?))
        };
        let (before, after) = text
            .split_once(',')
            .ok_or_else(|| format!("{text:?} is not two windows FROM..TO,FROM..TO"))?;
        Ok(Self::Windows([window(before)?, window(after)?]))
    }
}

/// What `kpi` looks at (decision 9).
#[derive(Debug, Clone)]
pub struct KpiQuery {
    pub period: Period,
    /// How many periods to list, the latest last.
    pub last: usize,
    /// The latest period listed is the one that holds this; now without.
    pub at: Option<Cursor>,
    /// One window of any length instead of the periods (`--since` /
    /// `--until`, the cursors of `stats`).
    pub since: Option<Cursor>,
    pub until: Option<Cursor>,
    /// The kinds whose strata are listed (every kind when empty), and the
    /// kinds a comparison's summary is made for (`runtime` when empty).
    pub kinds: Vec<String>,
    /// The axes the periods are split by besides the kind.
    pub by: Vec<Axis>,
    pub compare: Option<CompareSpec>,
    /// Each side of a comparison across a mark, in days.
    pub window_days: i64,
    /// Only the runs, asks and findings of this goal's tasks.
    pub goal_id: Option<GoalId>,
}

impl Default for KpiQuery {
    fn default() -> Self {
        Self {
            period: Period::Day,
            last: DEFAULT_LAST,
            at: None,
            since: None,
            until: None,
            kinds: Vec::new(),
            by: Vec::new(),
            compare: None,
            window_days: DEFAULT_WINDOW_DAYS,
            goal_id: None,
        }
    }
}

/// What `kpi` derives from.
#[derive(Debug, Clone, Copy)]
pub struct KpiInput<'a> {
    /// Every `run_events` row, ascending id.
    pub events: &'a [RunEvent],
    pub goals: &'a HashMap<TaskId, Option<GoalId>>,
    pub kinds: &'a HashMap<TaskId, Option<TaskKind>>,
    /// Unix seconds.
    pub now: i64,
    /// The host's time zone, seconds east of UTC.
    pub utc_offset_secs: i64,
    /// The host's logical cores, for the `load` axis.
    pub cores: Option<usize>,
    pub config: &'a KpiConfig,
}

/// One KPI over one stratum of one window: how many samples, and the value
/// (a count, rate or mean) or the spread (median, p90, min and max) of
/// them, or both. A value that could not be derived is null, not 0.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measure {
    pub n: usize,
    pub value: Option<f64>,
    pub median: Option<f64>,
    pub p90: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// The KPI has a value / a spread (serialized, null or not).
    has_value: bool,
    has_spread: bool,
    /// A count of the period itself: judged whatever `n` is.
    counted: bool,
}

impl Serialize for Measure {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("n", &self.n)?;
        if self.has_value {
            map.serialize_entry("value", &self.value)?;
        }
        if self.has_spread {
            map.serialize_entry("median", &self.median)?;
            map.serialize_entry("p90", &self.p90)?;
            map.serialize_entry("min", &self.min)?;
            map.serialize_entry("max", &self.max)?;
        }
        map.end()
    }
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[allow(clippy::cast_precision_loss)]
fn float(value: i64) -> f64 {
    value as f64
}

impl Measure {
    /// The spread of whole seconds, by `stats`' rules: the median of an
    /// even count is the floor of the mean of the middle two, the p90 the
    /// nearest rank.
    pub fn secs(values: impl IntoIterator<Item = i64>) -> Self {
        let mut values: Vec<i64> = values.into_iter().collect();
        Self {
            n: values.len(),
            median: median(&mut values).map(float),
            p90: p90(&mut values).map(float),
            min: values.first().copied().map(float),
            max: values.last().copied().map(float),
            has_spread: true,
            ..Self::default()
        }
    }

    /// The spread of values with fractions (loads, seconds of a command).
    pub fn spread(values: impl IntoIterator<Item = f64>) -> Self {
        let mut values: Vec<f64> = values.into_iter().collect();
        let middle = median_f64(&mut values);
        let rank = (values.len() * 9).div_ceil(10);
        Self {
            n: values.len(),
            median: middle,
            p90: rank.checked_sub(1).map(|index| round3(values[index])),
            min: values.first().copied().map(round3),
            max: values.last().copied().map(round3),
            has_spread: true,
            ..Self::default()
        }
    }

    /// `part / whole` over `whole` samples; null when `whole` is 0.
    pub fn ratio(part: f64, whole: usize) -> Self {
        Self {
            n: whole,
            #[allow(clippy::cast_precision_loss)]
            value: (whole > 0).then(|| round3(part / whole as f64)),
            has_value: true,
            ..Self::default()
        }
    }

    /// A count of the period (`landings`, `auto_repairs`).
    pub fn count(count: usize) -> Self {
        Self {
            n: count,
            #[allow(clippy::cast_precision_loss)]
            value: Some(count as f64),
            has_value: true,
            counted: true,
            ..Self::default()
        }
    }

    /// A value of the period that stands for itself (a total, a
    /// time-weighted mean) over `n` samples, null when there is none.
    pub fn total(value: Option<f64>, n: usize) -> Self {
        Self {
            n,
            value: value.map(round3),
            has_value: true,
            counted: true,
            ..Self::default()
        }
    }

    /// With a value next to the spread (`max_load_avg`: the maximum, and
    /// the claims' median and p90).
    fn with_value(mut self, value: Option<f64>) -> Self {
        self.value = value.map(round3);
        self.has_value = true;
        self
    }

    /// The statistic a target judges.
    pub fn stat(&self, stat: Stat) -> Option<f64> {
        match stat {
            Stat::Median => self.median,
            Stat::P90 => self.p90,
            Stat::Value => self.value,
        }
    }

    /// The statistic a target judges without naming one, and a comparison
    /// compares: the value, or the median of a spread without one.
    pub fn primary_stat(&self) -> Stat {
        if self.has_value {
            Stat::Value
        } else {
            Stat::Median
        }
    }

    pub fn primary(&self) -> Option<f64> {
        self.stat(self.primary_stat())
    }

    /// Enough samples to judge (decision 19): `min_samples`, or a count of
    /// the period.
    pub fn enough(&self, min_samples: usize) -> bool {
        self.counted || self.n >= min_samples
    }
}

/// Which way a KPI is better (decision 1's table); `None` for the ones
/// kept for reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Higher,
    Lower,
}

pub fn direction(kpi: &str) -> Option<Direction> {
    match kpi {
        "landings" | "slot_usage" | "first_pass_rate" => Some(Direction::Higher),
        "auto_repairs"
        | "improvement_proposals"
        | "candidates"
        | "plan.follow_up_adoption_rate" => None,
        _ if kpi.starts_with("session_active") => None,
        _ if kpi.starts_with("session_open.") => {
            (kpi == "session_open.worker").then_some(Direction::Lower)
        }
        _ => Some(Direction::Lower),
    }
}

/// How one KPI of one stratum moved from `before` to `after`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Change {
    pub previous: Option<f64>,
    pub delta: Option<f64>,
    pub ratio: Option<f64>,
    /// For a day: the median of the 7 days before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_7d: Option<f64>,
    /// Both sides have a value and enough samples; a change not judged
    /// shows neither `improved` nor `worsened`.
    pub judged: bool,
    /// Why not judged: `no_value`, `small_sample` or `partial`.
    pub reason: Option<&'static str>,
    /// `improved`, `worsened` or `unchanged` for a judged KPI with a good
    /// direction.
    pub verdict: Option<&'static str>,
}

impl Change {
    /// The period is not over: its value so far is shown next to the
    /// previous one, but not judged (a count is always short of it).
    pub fn not_over(&mut self) {
        self.judged = false;
        self.reason = Some("partial");
        self.verdict = None;
    }

    pub fn between(
        kpi: &str,
        before: Option<&Measure>,
        after: &Measure,
        min_samples: usize,
    ) -> Self {
        let previous = before.and_then(Measure::primary);
        let current = after.primary();
        let (delta, ratio) = match (previous, current) {
            (Some(previous), Some(current)) => (
                Some(round3(current - previous)),
                (previous != 0.0).then(|| round3(current / previous)),
            ),
            _ => (None, None),
        };
        let reason = if delta.is_none() {
            Some("no_value")
        } else if !(after.enough(min_samples) && before.is_some_and(|b| b.enough(min_samples))) {
            Some("small_sample")
        } else {
            None
        };
        let judged = reason.is_none();
        let verdict =
            judged
                .then(|| direction(kpi))
                .flatten()
                .zip(delta)
                .map(|(direction, delta)| match direction {
                    _ if delta == 0.0 => "unchanged",
                    Direction::Higher if delta > 0.0 => "improved",
                    Direction::Lower if delta < 0.0 => "improved",
                    _ => "worsened",
                });
        Self {
            previous,
            delta,
            ratio,
            baseline_7d: None,
            judged,
            reason,
            verdict,
        }
    }
}

/// Per KPI, per stratum.
pub type Kpis<T> = BTreeMap<String, BTreeMap<String, T>>;

/// One period's KPIs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PeriodKpis {
    /// `YYYY-MM-DD`, `YYYY-Www`, or `window` for `--since` / `--until`.
    pub label: String,
    pub start: String,
    pub end: String,
    /// The period is not over yet: its values are so far, and no target
    /// is judged on it.
    pub partial: bool,
    #[serde(flatten)]
    pub window: WindowKpis,
    /// The marks that took effect in the period.
    pub marks: Vec<Mark>,
    /// Each KPI next to the previous period's (and a day's 7-day baseline).
    pub comparison: Kpis<Change>,
}

/// The settings the KPIs were judged by, and where each came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfigReport {
    pub min_samples: usize,
    pub breach_periods: usize,
    pub breach_weeks: usize,
    pub max_improvement_proposals: usize,
    /// Per setting: `default`, `repository` (`dagq.toml`) or `host`
    /// (`host.toml`).
    pub sources: BTreeMap<&'static str, &'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Kpi {
    pub period: &'static str,
    pub utc_offset_secs: i64,
    pub cores: Option<usize>,
    pub config: ConfigReport,
    /// The periods listed, oldest first.
    pub periods: Vec<PeriodKpis>,
    /// Each target, judged over the periods (decision 18).
    pub targets: Vec<TargetReport>,
    /// The comparison `--compare` asked for.
    pub compare: Option<Comparison>,
}

/// The time a cursor stands for: its own, or the time of the latest event
/// at or before its id.
pub fn cursor_ms(cursor: Cursor, events: &[RunEvent]) -> Option<i64> {
    match cursor {
        Cursor::Time(ms) => Some(ms),
        Cursor::Event(id) => events
            .iter()
            .filter(|event| event.id <= id)
            .filter_map(|event| timestamp_millis(&event.created_at))
            .max(),
    }
}

/// A window of the KPIs, `start` exclusive and `end` inclusive like the
/// cursors of `stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: i64,
    end: i64,
}

/// The KPIs `query` asks for.
pub fn kpi(input: &KpiInput<'_>, query: &KpiQuery) -> Result<Kpi, String> {
    if query.last == 0 {
        return Err("--last must be at least 1".into());
    }
    let config = input.config;
    let context = Context::new(input, query.goal_id);
    let now_ms = input.now * 1000;
    let offset_ms = input.utc_offset_secs * 1000;
    let mut axes = vec![Axis::Kind];
    for axis in &query.by {
        if !axes.contains(axis) {
            axes.push(*axis);
        }
    }
    axes.sort_unstable();
    let window = query.since.is_some() || query.until.is_some();
    // Oldest first: the lookback, then the periods listed.
    let spans: Vec<(String, Span)> = if window {
        let end = match query.until {
            Some(until) => cursor_ms(until, input.events).ok_or("--until names no event")?,
            None => now_ms,
        };
        let start = match query.since {
            Some(since) => cursor_ms(since, input.events).ok_or("--since names no event")?,
            None => input
                .events
                .iter()
                .filter_map(|event| timestamp_millis(&event.created_at))
                .min()
                .unwrap_or(end)
                .saturating_sub(1),
        };
        if start >= end {
            return Err("--since must be before --until".into());
        }
        let length = end - start;
        vec![
            (
                "previous".to_owned(),
                Span {
                    start: start - length,
                    end: start,
                },
            ),
            ("window".to_owned(), Span { start, end }),
        ]
    } else {
        let anchor = match query.at {
            Some(at) => cursor_ms(at, input.events).ok_or("--at names no event")?,
            None => now_ms,
        };
        let latest = query.period.start(anchor, offset_ms);
        // Enough periods before the listed ones for a target's streak too.
        let breach_after = match query.period {
            Period::Day => config.breach_periods,
            Period::Week => config.breach_weeks,
        };
        let count = query.last + query.period.lookback().max(2 * breach_after);
        (0..count)
            .rev()
            .map(|back| {
                let start = latest - i64::try_from(back).unwrap_or(0) * query.period.length();
                // A week's length never changes, but a day read at a fixed
                // offset does not either: the period ends where the next starts.
                let end = start + query.period.length();
                (query.period.label(start, offset_ms), Span { start, end })
            })
            .collect()
    };
    let windows: Vec<WindowKpis> = spans
        .iter()
        .map(|(_, span)| context.window(span.start, span.end, &axes))
        .collect();
    let partial = |span: &Span| span.end > now_ms && !(window && query.until.is_some());
    let listed = if window { 1 } else { query.last };
    let first_listed = spans.len() - listed;
    let mut periods = Vec::new();
    for index in first_listed..spans.len() {
        let (label, span) = &spans[index];
        let current = &windows[index];
        let mut comparison: Kpis<Change> = BTreeMap::new();
        for (name, strata) in &current.kpis {
            for (stratum, measure) in strata {
                let previous = index
                    .checked_sub(1)
                    .and_then(|previous| windows[previous].kpis.get(name)?.get(stratum));
                let mut change = Change::between(name, previous, measure, config.min_samples);
                if partial(span) {
                    change.not_over();
                }
                if query.period == Period::Day && !window {
                    let mut days: Vec<f64> = windows[index.saturating_sub(BASELINE_DAYS)..index]
                        .iter()
                        .filter_map(|day| day.kpis.get(name)?.get(stratum)?.primary())
                        .collect();
                    change.baseline_7d = median_f64(&mut days);
                }
                comparison
                    .entry(name.clone())
                    .or_default()
                    .insert(stratum.clone(), change);
            }
        }
        periods.push(PeriodKpis {
            label: label.clone(),
            start: marks::utc_text(span.start),
            end: marks::utc_text(span.end),
            partial: partial(span),
            window: current.clone(),
            marks: marks::marks(
                input.events,
                Some(Cursor::Time(span.start)),
                Some(Cursor::Time(span.end)),
            ),
            comparison,
        });
    }
    let judged: Vec<config::JudgedPeriod<'_>> = spans
        .iter()
        .zip(&windows)
        .enumerate()
        .filter(|(index, _)| !window || *index >= first_listed)
        .map(|(index, ((label, span), kpis))| config::JudgedPeriod {
            label,
            kpis: &kpis.kpis,
            partial: partial(span),
            listed: index >= first_listed,
        })
        .collect();
    let breach_after = match query.period {
        Period::Day => config.breach_periods,
        Period::Week => config.breach_weeks,
    };
    let targets = config::judge(config, &judged, breach_after);
    let compare = query
        .compare
        .map(|spec| compare::compare(&context, spec, query, now_ms))
        .transpose()?;
    let mut kpi = Kpi {
        period: if window {
            "window"
        } else {
            query.period.as_str()
        },
        utc_offset_secs: input.utc_offset_secs,
        cores: input.cores,
        config: config.report(),
        periods,
        targets,
        compare,
    };
    if !query.kinds.is_empty() {
        let keep = |stratum: &String| {
            stratum
                .strip_prefix("kind=")
                .is_none_or(|kind| query.kinds.iter().any(|wanted| wanted == kind))
        };
        for period in &mut kpi.periods {
            for strata in period.window.kpis.values_mut() {
                strata.retain(|stratum, _| keep(stratum));
            }
            for strata in period.comparison.values_mut() {
                strata.retain(|stratum, _| keep(stratum));
            }
        }
        if let Some(compare) = &mut kpi.compare {
            for strata in compare.strata.values_mut() {
                strata.retain(|stratum, _| keep(stratum));
            }
        }
    }
    Ok(kpi)
}

#[cfg(test)]
mod tests;
